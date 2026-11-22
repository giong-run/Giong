//! `giong clean`: free the disk old builds take, without making the next build
//! start from nothing.
//!
//! Cargo never deletes a build it no longer uses. Each Rust update leaves every
//! dependency built with the old compiler behind, and incremental sessions
//! pile up. This removes, inside build directories cargo made:
//!
//! - every unit built by another compiler than the one the project uses now
//!   (found from cargo's fingerprints, compared with a one-file probe build
//!   run in the project, so its `rust-toolchain.toml` counts);
//! - with `--days N`, every unit not used (built or read) for N days;
//! - incremental-compilation data not touched for 14 days (`--days` too).
//!
//! `--all` empties the whole build directory, as `cargo clean` does.
//!
//! A directory is cargo's when its `CACHEDIR.TAG` says cargo created it, or it
//! holds cargo's `.rustc_info.json`; the generic tag alone (pytest, backup
//! tools) is never enough. Nothing outside such a directory is touched, and
//! no link is followed.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};

use super::doctor::dir_size;
use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::host;
use crate::project::{cargo, cargo_missing, find_manifest_dir, first_error, Project};
use crate::report::{Failure, Finding, Report, Severity};

pub const COMMAND: Command = Command {
    name: "clean",
    summary: "free the disk old builds use, keeping what the next build needs",
    usage: "giong clean [dir...] [--days N] [--all] [--dry-run]\n\n\
            Removes what cargo built with an older Rust, and incremental data unused for 14 days, \
            from this project's build directory, or from every cargo build directory under the \
            directories given (giong clean ~/code). --days N also removes units not used for N \
            days. --all empties the whole build directory. Only directories cargo made are \
            touched.",
    flags: &[
        Flag {
            name: "days",
            kind: Kind::Value,
            help: "also remove what was not used for this many days",
        },
        Flag {
            name: "all",
            kind: Kind::Switch,
            help: "empty the whole build directory (the next build starts from nothing)",
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
const CARGO_TAG_LINE: &str = "created by cargo";

/// One build directory to sweep.
struct Target {
    /// The target directory (what `--all` empties).
    dir: PathBuf,
    /// `build.build-dir`, when it is somewhere else.
    build_dir: Option<PathBuf>,
    /// The project it belongs to, for the compiler probe: `None` when it
    /// cannot be told (a target directory shared by several projects).
    project: Option<PathBuf>,
}

fn run(args: &Args) -> Result<Report, Failure> {
    let days: Option<u64> = args.number("days")?;
    let all = args.has("all");
    let dry = args.has("dry-run");
    let mut r = Report::new("clean");
    let mut targets: Vec<Target> = Vec::new();
    if args.words.is_empty() {
        let p = Project::here()?;
        if p.target_dir.is_dir() && is_cargo_build(&p.target_dir, true) {
            let build_dir =
                (p.build_dir != p.target_dir && p.build_dir.is_dir()).then(|| p.build_dir.clone());
            targets.push(Target {
                dir: p.target_dir,
                build_dir,
                project: Some(p.root),
            });
        } else if p.target_dir.exists() {
            r.push(
                Finding::new(
                    Code::DISK_USED,
                    Severity::Warning,
                    format!(
                        "{} does not look like a cargo build directory, so it was left alone",
                        p.target_dir.display()
                    ),
                )
                .fix("build once (cargo build) so cargo marks it, or remove it yourself"),
            );
        }
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
        let mut seen = HashSet::new();
        for dir in found {
            let real = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
            if !seen.insert(real.clone()) {
                continue;
            }
            let project = real
                .parent()
                .filter(|p| p.join("Cargo.toml").is_file())
                .map(Path::to_path_buf);
            targets.push(Target {
                dir: real,
                build_dir: None,
                project,
            });
        }
    }
    let mut total_freed = 0u64;
    let mut rows = Vec::new();
    // One probe per toolchain, not per project.
    let mut probes: HashMap<String, Option<u64>> = HashMap::new();
    for target in &targets {
        let dirs: Vec<&Path> = std::iter::once(target.dir.as_path())
            .chain(target.build_dir.as_deref())
            .collect();
        let before: u64 = dirs.iter().map(|d| dir_size(d)).sum();
        let plan = if all {
            Plan {
                paths: dirs.iter().flat_map(|d| contents(d)).collect(),
                reason: "whole build directory",
            }
        } else {
            let Some(project) = &target.project else {
                r.push(
                    Finding::new(
                        Code::DISK_USED,
                        Severity::Info,
                        format!(
                            "{} was skipped: its project is not known, so neither is its compiler",
                            target.dir.display()
                        ),
                    )
                    .fix("run giong clean inside the project, or use --all to empty it"),
                );
                continue;
            };
            let rustc = match probe_cached(&mut probes, project) {
                Ok(h) => h,
                Err(f) => {
                    r.push(
                        Finding::new(
                            f.code,
                            Severity::Warning,
                            format!("{} was skipped: {}", target.dir.display(), f.what),
                        )
                        .fix(f.fix),
                    );
                    continue;
                }
            };
            let mut paths = Vec::new();
            for d in &dirs {
                paths.extend(plan(d, rustc, days).paths);
            }
            Plan {
                paths,
                reason: match days {
                    Some(_) => "older compiler or not used recently",
                    None => "built by an older compiler, or stale incremental data",
                },
            }
        };
        let freed = if dry {
            removable_size(&plan.paths)
        } else {
            for path in &plan.paths {
                let is_dir = fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
                let res = if is_dir {
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
                }
            }
            let after: u64 = dirs.iter().map(|d| dir_size(d)).sum();
            before.saturating_sub(after)
        };
        total_freed += freed;
        r.line(format!(
            "{}{}: {} of {} ({} items, {})",
            if dry { "would free " } else { "freed " },
            target.dir.display(),
            human(freed),
            human(before),
            plan.paths.len(),
            plan.reason
        ));
        rows.push(json!({
            "target_dir": target.dir.display().to_string(),
            "build_dir": target.build_dir.as_ref().map(|d| d.display().to_string()),
            "bytes_before": before,
            "bytes_freed": freed,
            "items": plan.paths.len(),
        }));
    }
    if rows.is_empty() && r.findings().is_empty() {
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

/// The entries of `dir`, except its `CACHEDIR.TAG` (kept, so the emptied
/// directory is still known as cargo's).
fn contents(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name() != "CACHEDIR.TAG")
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

/// What removing `paths` would free: a file with other links stays on disk.
fn removable_size(paths: &[PathBuf]) -> u64 {
    let mut total = 0;
    let mut stack: Vec<PathBuf> = paths.to_vec();
    while let Some(p) = stack.pop() {
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        if meta.is_dir() {
            if let Ok(entries) = fs::read_dir(&p) {
                stack.extend(entries.flatten().map(|e| e.path()));
            }
        } else if meta.is_file() && only_link(&meta) {
            total += meta.len();
        }
    }
    total
}

#[cfg(unix)]
fn only_link(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    meta.nlink() < 2
}

#[cfg(not(unix))]
fn only_link(_: &fs::Metadata) -> bool {
    true
}

/// The compiler hash for `project`, probing once per toolchain (`rustc -vV`
/// run in the project names it, rust-toolchain.toml included).
fn probe_cached(
    probes: &mut HashMap<String, Option<u64>>,
    project: &Path,
) -> Result<Option<u64>, Failure> {
    let key = host::output_in(project, "rustc", &["-vV"]).unwrap_or_default();
    if !key.is_empty() {
        if let Some(h) = probes.get(&key) {
            return Ok(*h);
        }
    }
    let h = current_rustc_hash(project)?;
    if !key.is_empty() {
        probes.insert(key, h);
    }
    Ok(h)
}

/// What to remove from one build directory.
pub struct Plan {
    pub paths: Vec<PathBuf>,
    pub reason: &'static str,
}

fn days_to_duration(days: u64) -> Duration {
    Duration::from_secs(days.saturating_mul(86_400))
}

/// When a file was last written or read (read times are kept to the day by
/// most mounts, which is enough here); `None` when the system cannot say.
fn last_use(meta: &fs::Metadata) -> Option<SystemTime> {
    let modified = meta.modified().ok();
    let accessed = meta.accessed().ok();
    modified.max(accessed)
}

/// Units built by another compiler (or not used for `days`), and stale
/// incremental data, in every profile directory of `target`.
pub fn plan(target: &Path, current_rustc: Option<u64>, days: Option<u64>) -> Plan {
    let now = SystemTime::now();
    let mut paths = Vec::new();
    for profile in profile_dirs(target) {
        // Every unit: its compiler, and when any of its files was last used.
        let mut rustc: HashMap<String, Option<u64>> = HashMap::new();
        let mut used: HashMap<String, SystemTime> = HashMap::new();
        let mut note_use = |hash: &str, path: &Path| {
            let mut stack = vec![path.to_path_buf()];
            while let Some(p) = stack.pop() {
                let Ok(meta) = fs::symlink_metadata(&p) else {
                    continue;
                };
                if meta.is_dir() {
                    if let Ok(entries) = fs::read_dir(&p) {
                        stack.extend(entries.flatten().map(|e| e.path()));
                    }
                }
                // Unknown or in the future: as good as used now.
                let t = last_use(&meta).filter(|t| *t <= now).unwrap_or(now);
                let e = used.entry(hash.to_string()).or_insert(t);
                if t > *e {
                    *e = t;
                }
            }
        };
        if let Ok(units) = fs::read_dir(profile.join(".fingerprint")) {
            for unit in units.flatten() {
                let name = unit.file_name().to_string_lossy().into_owned();
                let Some(hash) = unit_hash(&name) else {
                    continue;
                };
                rustc.insert(hash.to_string(), unit_rustc(&unit.path()));
                note_use(hash, &unit.path());
            }
        }
        if days.is_some() {
            for sub in ["deps", "build"] {
                for e in fs::read_dir(profile.join(sub))
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let stem = name.split('.').next().unwrap_or(&name);
                    if let Some(h) = unit_hash(stem) {
                        if rustc.contains_key(h) {
                            note_use(h, &e.path());
                        }
                    }
                }
            }
        }
        let stale: HashSet<&String> = rustc
            .iter()
            .filter(|(hash, then)| {
                let other_compiler = matches!((current_rustc, then), (Some(n), Some(t)) if n != *t);
                let unused = days.is_some_and(|d| {
                    used.get(*hash)
                        .and_then(|t| now.duration_since(*t).ok())
                        .is_some_and(|age| age > days_to_duration(d))
                });
                other_compiler || unused
            })
            .map(|(h, _)| h)
            .collect();
        if !stale.is_empty() {
            for sub in [".fingerprint", "build", "deps", "examples"] {
                for e in fs::read_dir(profile.join(sub))
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let stem = name.split('.').next().unwrap_or(&name);
                    if unit_hash(stem).is_some_and(|h| stale.contains(&h.to_string())) {
                        paths.push(e.path());
                    }
                }
            }
        }
        let incremental_days = days.unwrap_or(INCREMENTAL_DAYS);
        for e in fs::read_dir(profile.join("incremental"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if newest_mtime_older(&e.path(), now, incremental_days) {
                paths.push(e.path());
            }
        }
    }
    paths.sort();
    Plan {
        paths,
        reason: match days {
            Some(_) => "older compiler or not used recently",
            None => "built by an older compiler, or stale incremental data",
        },
    }
}

/// Whether everything under `dir` is older than `days`.
fn newest_mtime_older(dir: &Path, now: SystemTime, days: u64) -> bool {
    let limit = days_to_duration(days);
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
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if p.join(".fingerprint").is_dir() {
            out.push(p);
        } else if let Ok(inner) = fs::read_dir(&p) {
            for i in inner.flatten() {
                if i.file_type().is_ok_and(|t| t.is_dir()) && i.path().join(".fingerprint").is_dir()
                {
                    out.push(i.path());
                }
            }
        }
    }
    out.sort();
    out
}

/// Whether cargo made `dir`: its `CACHEDIR.TAG` says cargo created it, or it
/// holds cargo's `.rustc_info.json`. `known` (the directory cargo metadata
/// names for the project) also accepts a directory made before the first
/// build, which cargo does not tag, once it holds a profile with fingerprints.
pub fn is_cargo_build(dir: &Path, known: bool) -> bool {
    let tagged = fs::read_to_string(dir.join("CACHEDIR.TAG"))
        .map(|t| t.starts_with(TAG) && t.contains(CARGO_TAG_LINE))
        .unwrap_or(false);
    tagged || dir.join(".rustc_info.json").is_file() || (known && !profile_dirs(dir).is_empty())
}

/// Cargo build directories under `dir`, not descending into them, `.git` or
/// `node_modules`, at most 8 levels down, never through a link.
pub fn find_target_dirs(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > 8 {
        return;
    }
    if is_cargo_build(dir, false) {
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
        .env("CARGO_BUILD_BUILD_DIR", probe.join("target"))
        .env_remove("RUSTFLAGS")
        .current_dir(find_manifest_dir(project_dir).unwrap_or_else(|| project_dir.to_path_buf()))
        .output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            let _ = fs::remove_dir_all(&probe);
            return Err(cargo_missing(e));
        }
    };
    if !out.status.success() {
        let _ = fs::remove_dir_all(&probe);
        return Err(Failure::new(
            Code::CARGO_FAILED,
            format!(
                "a probe build to identify the compiler failed: {}",
                first_error(&String::from_utf8_lossy(&out.stderr))
            ),
            "check that cargo build works here (the project's toolchain may not be installed); \
             giong clean --all needs no probe",
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

    const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n\
        # This file is a cache directory tag created by cargo.\n";

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
                ("CACHEDIR.TAG", CARGO_TAG),
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
        assert!(is_cargo_build(&t, false));
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
                ("a/target/CACHEDIR.TAG", CARGO_TAG),
                ("b/c/target/.rustc_info.json", "{}"),
                ("f/.pytest_cache/CACHEDIR.TAG", TAG),
                ("d/target/CACHEDIR.TAG", "not cargo's"),
                ("e/node_modules/x/target/CACHEDIR.TAG", CARGO_TAG),
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
