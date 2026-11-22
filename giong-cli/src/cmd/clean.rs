//! `giong clean`: free the disk old builds take, without making the next build
//! start from nothing.
//!
//! Cargo never deletes a build it no longer uses. Each Rust update leaves every
//! dependency built with the old compiler behind, and incremental sessions
//! pile up. This removes, inside build directories cargo made (they carry its
//! `CACHEDIR.TAG`):
//!
//! - every unit built by another compiler than the one this project uses now
//!   (found from cargo's fingerprints, compared with a one-file probe build);
//! - with `--days N`, every unit not rebuilt for N days;
//! - incremental-compilation data not touched for 14 days (`--days` too).
//!
//! `--all` removes the whole build directory, as `cargo clean` does. Nothing
//! outside a directory with cargo's tag is ever touched.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use super::doctor::dir_size;
use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::project::{cargo, cargo_missing, first_error, Project};
use crate::report::{Failure, Finding, Report, Severity};

pub const COMMAND: Command = Command {
    name: "clean",
    summary: "free the disk old builds use, keeping what the next build needs",
    usage: "giong clean [dir...] [--days N] [--all] [--dry-run]\n\n\
            Removes what cargo built with an older Rust, and incremental data unused for 14 days, \
            from this project's build directory, or from every cargo build directory under the \
            directories given (giong clean ~/code). --days N also removes units not rebuilt for N \
            days. --all removes the whole build directory. Only directories cargo marked as its \
            own (CACHEDIR.TAG) are touched.",
    flags: &[
        Flag {
            name: "days",
            kind: Kind::Value,
            help: "also remove what was not rebuilt for this many days",
        },
        Flag {
            name: "all",
            kind: Kind::Switch,
            help: "remove the whole build directory (the next build starts from nothing)",
        },
        Flag {
            name: "dry-run",
            kind: Kind::Switch,
            help: "say what would be removed, remove nothing",
        },
    ],
    max_words: 16,
    run,
};

const INCREMENTAL_DAYS: u64 = 14;
const TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55";

fn run(args: &Args) -> Result<Report, Failure> {
    let days: Option<u64> = args.number("days")?;
    let all = args.has("all");
    let dry = args.has("dry-run");
    let targets: Vec<PathBuf> = if args.words.is_empty() {
        let p = Project::here()?;
        vec![p.target_dir]
    } else {
        let mut found = Vec::new();
        for w in &args.words {
            let dir = Path::new(w);
            if !dir.is_dir() {
                return Err(Failure::new(
                    Code::USAGE,
                    format!("{w} is not a directory"),
                    "give directories that hold projects, such as giong clean ~/code",
                ));
            }
            find_target_dirs(dir, 0, &mut found);
        }
        found
    };
    let mut r = Report::new("clean");
    let mut total_freed = 0u64;
    let mut rows = Vec::new();
    for target in &targets {
        if !is_cargo_dir(target) {
            continue;
        }
        let before = dir_size(target);
        let plan = if all {
            Plan {
                paths: vec![target.clone()],
                reason: "whole build directory",
            }
        } else {
            let rustc = current_rustc_hash(target.parent().unwrap_or(target))?;
            plan(target, rustc, days)
        };
        let mut freed = 0u64;
        for path in &plan.paths {
            let size = if path.is_dir() {
                dir_size(path)
            } else {
                fs::symlink_metadata(path).map(|m| m.len()).unwrap_or(0)
            };
            if !dry {
                let res = if path.is_dir() {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                };
                if let Err(e) = res {
                    r.push(
                        Finding::new(
                            Code::DISK_USED,
                            Severity::Warning,
                            format!("{} could not be removed", path.display()),
                        )
                        .detail(e.to_string())
                        .fix("check the permissions, or close the program that holds it"),
                    );
                    continue;
                }
            }
            freed += size;
        }
        total_freed += freed;
        r.line(format!(
            "{}{}: {} of {} ({} items, {})",
            if dry { "would free " } else { "freed " },
            target.display(),
            human(freed),
            human(before),
            plan.paths.len(),
            plan.reason
        ));
        rows.push(json!({
            "target_dir": target.display().to_string(),
            "bytes_before": before,
            "bytes_freed": freed,
            "items": plan.paths.len(),
        }));
    }
    if rows.is_empty() {
        r.line("No cargo build directory found.");
    } else if rows.len() > 1 {
        r.line(format!(
            "{} {} in all",
            if dry { "Would free" } else { "Freed" },
            human(total_freed)
        ));
    }
    r.data("dry_run", json!(dry));
    r.data("bytes_freed", json!(total_freed));
    r.data("targets", Value::Array(rows));
    Ok(r)
}

/// What to remove from one build directory.
pub struct Plan {
    pub paths: Vec<PathBuf>,
    pub reason: &'static str,
}

/// Units built by another compiler (or older than `days`), and stale
/// incremental data, in every profile directory of `target`.
pub fn plan(target: &Path, current_rustc: Option<u64>, days: Option<u64>) -> Plan {
    let now = SystemTime::now();
    let older = |path: &Path, days: u64| -> bool {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > Duration::from_secs(days * 86_400))
    };
    let mut paths = Vec::new();
    for profile in profile_dirs(target) {
        let mut stale: HashSet<String> = HashSet::new();
        if let Ok(units) = fs::read_dir(profile.join(".fingerprint")) {
            for unit in units.flatten() {
                let name = unit.file_name().to_string_lossy().into_owned();
                let Some(hash) = unit_hash(&name) else {
                    continue;
                };
                let other_compiler = match (current_rustc, unit_rustc(&unit.path())) {
                    (Some(now), Some(then)) => now != then,
                    _ => false,
                };
                let too_old = days.is_some_and(|d| older(&unit.path(), d));
                if other_compiler || too_old {
                    stale.insert(hash.to_string());
                }
            }
        }
        if !stale.is_empty() {
            for sub in [".fingerprint", "build", "deps", "examples"] {
                let Ok(entries) = fs::read_dir(profile.join(sub)) else {
                    continue;
                };
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let stem = name.split('.').next().unwrap_or(&name);
                    if unit_hash(stem).is_some_and(|h| stale.contains(h)) {
                        paths.push(e.path());
                    }
                }
            }
        }
        let incremental_days = days.unwrap_or(INCREMENTAL_DAYS);
        if let Ok(entries) = fs::read_dir(profile.join("incremental")) {
            for e in entries.flatten() {
                if newest_mtime_older(&e.path(), now, incremental_days) {
                    paths.push(e.path());
                }
            }
        }
    }
    paths.sort();
    Plan {
        paths,
        reason: match days {
            Some(_) => "older compiler or not rebuilt recently",
            None => "built by an older compiler, or stale incremental data",
        },
    }
}

/// Whether everything under `dir` is older than `days`.
fn newest_mtime_older(dir: &Path, now: SystemTime, days: u64) -> bool {
    let limit = Duration::from_secs(days * 86_400);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&d) else {
            continue;
        };
        if let Some(age) = meta
            .modified()
            .ok()
            .and_then(|t| now.duration_since(t).ok())
        {
            if age <= limit {
                return false;
            }
        } else {
            return false; // a time in the future, or unknown: keep it
        }
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&d) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        }
    }
    true
}

/// `serde-0123456789abcdef` -> `0123456789abcdef`.
pub fn unit_hash(name: &str) -> Option<&str> {
    let (_, hash) = name.rsplit_once('-')?;
    (hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hash)
}

/// The compiler hash (`"rustc": N`) of a fingerprint unit directory.
pub fn unit_rustc(unit: &Path) -> Option<u64> {
    for e in fs::read_dir(unit).ok()?.flatten() {
        if e.path().extension().is_some_and(|x| x == "json") {
            let text = fs::read_to_string(e.path()).ok()?;
            let v: Value = serde_json::from_str(&text).ok()?;
            if let Some(n) = v["rustc"].as_u64() {
                return Some(n);
            }
        }
    }
    None
}

/// Directories under `target` that hold a `.fingerprint` (debug, release,
/// custom profiles, and the same under `<target-triple>/`).
pub fn profile_dirs(target: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(target) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if p.join(".fingerprint").is_dir() {
            out.push(p);
        } else if let Ok(inner) = fs::read_dir(&p) {
            for i in inner.flatten() {
                if i.path().join(".fingerprint").is_dir() {
                    out.push(i.path());
                }
            }
        }
    }
    out.sort();
    out
}

pub fn is_cargo_dir(dir: &Path) -> bool {
    fs::read_to_string(dir.join("CACHEDIR.TAG"))
        .map(|t| t.starts_with(TAG))
        .unwrap_or(false)
}

/// Cargo build directories under `dir` (a `target` with cargo's tag), not
/// descending into them, `.git` or `node_modules`, at most 8 levels down.
pub fn find_target_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    if is_cargo_dir(dir) {
        out.push(dir.to_path_buf());
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if name == ".git" || name == "node_modules" {
            continue;
        }
        if e.file_type().is_ok_and(|t| t.is_dir()) {
            find_target_dirs(&e.path(), depth + 1, out);
        }
    }
}

/// The compiler hash cargo writes into fingerprints for the toolchain that
/// `project_dir` uses (its rust-toolchain file counts): a one-file crate is
/// built in a temporary directory and its fingerprint read.
pub fn current_rustc_hash(project_dir: &Path) -> Result<Option<u64>, Failure> {
    let probe = std::env::temp_dir().join(format!("giong-clean-probe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&probe);
    fs::create_dir_all(probe.join("src")).map_err(probe_failed)?;
    fs::write(
        probe.join("Cargo.toml"),
        "[package]\nname = \"giong-clean-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .map_err(probe_failed)?;
    fs::write(probe.join("src/lib.rs"), "").map_err(probe_failed)?;
    let out = cargo()
        .args(["build", "--quiet", "--offline", "--manifest-path"])
        .arg(probe.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", probe.join("target"))
        .env_remove("RUSTFLAGS")
        .current_dir(project_dir)
        .output()
        .map_err(cargo_missing)?;
    if !out.status.success() {
        let _ = fs::remove_dir_all(&probe);
        return Err(Failure::new(
            Code::CARGO_FAILED,
            format!(
                "a probe build to identify the compiler failed: {}",
                first_error(&String::from_utf8_lossy(&out.stderr))
            ),
            "check that cargo build works here; giong clean --all needs no probe",
        ));
    }
    let mut hash = None;
    for profile in profile_dirs(&probe.join("target")) {
        if let Ok(units) = fs::read_dir(profile.join(".fingerprint")) {
            for u in units.flatten() {
                if let Some(h) = unit_rustc(&u.path()) {
                    hash = Some(h);
                }
            }
        }
    }
    let _ = fs::remove_dir_all(&probe);
    Ok(hash)
}

fn probe_failed(e: std::io::Error) -> Failure {
    Failure::new(
        Code::CARGO_FAILED,
        format!("the temporary directory cannot be written: {e}"),
        "free space in the temporary directory, or set TMPDIR to one that is writable",
    )
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path, files: &[(&str, &str)]) {
        for (path, body) in files {
            let p = root.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("giong-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn units_of_another_compiler_go_and_the_rest_stays() {
        let t = temp("clean-plan");
        let old = "1111111111111111";
        let new = "2222222222222222";
        tree(
            &t,
            &[
                ("CACHEDIR.TAG", TAG),
                (
                    &format!("debug/.fingerprint/serde-{old}/lib-serde.json"),
                    r#"{"rustc":1}"#,
                ),
                (
                    &format!("debug/.fingerprint/serde-{new}/lib-serde.json"),
                    r#"{"rustc":2}"#,
                ),
                (&format!("debug/deps/libserde-{old}.rlib"), "old"),
                (&format!("debug/deps/libserde-{old}.rmeta"), "old"),
                (&format!("debug/deps/serde-{old}.d"), "old"),
                (&format!("debug/deps/libserde-{new}.rlib"), "new"),
                (&format!("debug/build/serde-{old}/out/x"), "old"),
                ("debug/deps/app", "a binary without a hash"),
                (
                    &format!("x86_64-unknown-linux-gnu/release/.fingerprint/a-{old}/bin-a.json"),
                    r#"{"rustc":1}"#,
                ),
                (
                    &format!("x86_64-unknown-linux-gnu/release/deps/a-{old}"),
                    "old bin",
                ),
            ],
        );
        assert!(is_cargo_dir(&t));
        let p = plan(&t, Some(2), None);
        let names: Vec<String> = p
            .paths
            .iter()
            .map(|x| x.strip_prefix(&t).unwrap().display().to_string())
            .collect();
        assert_eq!(
            names,
            [
                format!("debug/.fingerprint/serde-{old}"),
                format!("debug/build/serde-{old}"),
                format!("debug/deps/libserde-{old}.rlib"),
                format!("debug/deps/libserde-{old}.rmeta"),
                format!("debug/deps/serde-{old}.d"),
                format!("x86_64-unknown-linux-gnu/release/.fingerprint/a-{old}"),
                format!("x86_64-unknown-linux-gnu/release/deps/a-{old}"),
            ]
        );
        // Unknown compiler: nothing is removed.
        assert!(plan(&t, None, None).paths.is_empty());
        // --days 0 removes every unit (all are older than 0 days).
        std::thread::sleep(Duration::from_millis(1100));
        let p = plan(&t, Some(2), Some(0));
        assert!(p
            .paths
            .iter()
            .any(|x| x.ends_with(format!("libserde-{new}.rlib"))));
        assert!(!p.paths.iter().any(|x| x.ends_with("app")));
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn only_tagged_directories_are_found() {
        let t = temp("clean-find");
        tree(
            &t,
            &[
                ("a/target/CACHEDIR.TAG", TAG),
                ("b/c/target/CACHEDIR.TAG", TAG),
                ("d/target/CACHEDIR.TAG", "not cargo's"),
                ("e/node_modules/x/target/CACHEDIR.TAG", TAG),
            ],
        );
        let mut found = Vec::new();
        find_target_dirs(&t, 0, &mut found);
        found.sort();
        assert_eq!(found, [t.join("a/target"), t.join("b/c/target")]);
        fs::remove_dir_all(&t).unwrap();
    }

    #[test]
    fn hashes_and_sizes() {
        assert_eq!(
            unit_hash("serde-0123456789abcdef"),
            Some("0123456789abcdef")
        );
        assert_eq!(unit_hash("build-script-build"), None);
        assert_eq!(unit_hash("a-0123456789abcdeg"), None);
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1536), "1.5 KiB");
        assert_eq!(human(3 << 30), "3.0 GiB");
    }

    #[test]
    fn the_probe_finds_the_compiler_hash() {
        let h = current_rustc_hash(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        assert!(h.is_some());
        // The same toolchain gives the same hash.
        assert_eq!(
            h,
            current_rustc_hash(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap()
        );
    }
}
