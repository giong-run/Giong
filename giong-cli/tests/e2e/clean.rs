//! `giong clean` on real cargo build directories: it removes only what it
//! says, never outside a directory cargo tagged, and the next build is not a
//! build from nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::harness::{giong, new_package, TempDir};

fn cargo() -> std::process::Command {
    let mut c = std::process::Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()));
    c.env_remove("CARGO_TARGET_DIR").env_remove("RUSTFLAGS");
    c
}

fn build(dir: &Path) -> String {
    let out = cargo()
        .args(["build", "--offline", "-v"])
        .current_dir(dir)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "cargo build failed:\n{err}");
    err
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                stack.push(p.clone());
            }
            out.push(p);
        }
    }
    out.sort();
    out
}

/// Cargo's two layouts of a build directory: up to Rust 1.9x every unit's
/// fingerprint sits in `<profile>/.fingerprint/<name>-<hash>/` and its
/// outputs in `<profile>/deps/`; newer cargo keeps each unit in
/// `<profile>/build/<name>/<hash>/` (`fingerprint/`, `out/`, ...). The tests
/// run on whichever the toolchain writes.
fn new_layout(profile: &Path) -> bool {
    !profile.join(".fingerprint").is_dir()
}

/// The directories that hold a unit's fingerprint JSON, in either layout.
fn unit_dirs(profile: &Path) -> Vec<PathBuf> {
    if new_layout(profile) {
        let mut out = Vec::new();
        for pkg in std::fs::read_dir(profile.join("build"))
            .into_iter()
            .flatten()
            .flatten()
        {
            for unit in std::fs::read_dir(pkg.path())
                .into_iter()
                .flatten()
                .flatten()
            {
                if unit.path().join("fingerprint").is_dir() {
                    out.push(unit.path());
                }
            }
        }
        out
    } else {
        std::fs::read_dir(profile.join(".fingerprint"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect()
    }
}

fn copy_tree(from: &Path, to: &Path, edit_json: &dyn Fn(&mut Value)) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let src = e.path();
        let dst = to.join(e.file_name());
        if src.is_dir() {
            copy_tree(&src, &dst, edit_json);
            continue;
        }
        let mut bytes = std::fs::read(&src).unwrap();
        if src.extension().is_some_and(|x| x == "json") {
            if let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) {
                edit_json(&mut v);
                bytes = serde_json::to_vec(&v).unwrap();
            }
        }
        std::fs::write(dst, bytes).unwrap();
    }
}

/// A unit that looks like one an older compiler built: a copy of a real unit
/// under a new hash, with another `rustc` value, and an output of 4 KiB.
/// Returns every path that must be gone after `giong clean`.
fn fake_old_unit(profile: &Path, name: &str, hash: &str) -> Vec<PathBuf> {
    let other_rustc = |v: &mut Value| {
        if let Some(n) = v["rustc"].as_u64() {
            v["rustc"] = Value::from(n.wrapping_add(1));
        }
    };
    if new_layout(profile) {
        let real = profile.join("build").join(name);
        let real = std::fs::read_dir(&real)
            .unwrap_or_else(|e| panic!("no unit of {name} in {}: {e}", real.display()))
            .flatten()
            .find(|e| e.path().join("fingerprint").is_dir())
            .expect("a real unit")
            .path();
        let fake = profile.join("build").join(name).join(hash);
        copy_tree(&real, &fake, &other_rustc);
        std::fs::create_dir_all(fake.join("out")).unwrap();
        std::fs::write(fake.join("out/lib.rlib"), vec![0u8; 4096]).unwrap();
        return vec![fake];
    }
    let fp = profile.join(".fingerprint");
    let real = std::fs::read_dir(&fp)
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with(name))
        .expect("a real unit")
        .path();
    let fake = fp.join(format!("{name}-{hash}"));
    copy_tree(&real, &fake, &other_rustc);
    let lib = profile.join(format!("deps/lib{name}-{hash}.rlib"));
    let dep = profile.join(format!("deps/{name}-{hash}.d"));
    std::fs::write(&lib, vec![0u8; 4096]).unwrap();
    std::fs::write(&dep, "x").unwrap();
    vec![fake, lib, dep]
}

/// Where a stale unit's output may be a symlink, in either layout.
fn stale_output_path(profile: &Path, name: &str, hash: &str, stale: &[PathBuf]) -> PathBuf {
    if new_layout(profile) {
        stale[0].join("out/linked")
    } else {
        profile.join(format!("build/{name}-{hash}"))
    }
}

fn set_old(path: &Path, days: u64) {
    let t = SystemTime::now() - Duration::from_secs(days * 86_400);
    for p in std::iter::once(path.to_path_buf()).chain(files_under(path)) {
        if p.is_file() {
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(t).unwrap();
        }
    }
    if path.is_dir() {
        // Directories last, as writing files touched them.
        let mut dirs: Vec<PathBuf> = files_under(path)
            .into_iter()
            .filter(|p| p.is_dir())
            .collect();
        dirs.push(path.to_path_buf());
        for d in dirs.into_iter().rev() {
            if let Ok(f) = std::fs::File::open(&d) {
                let _ = f.set_modified(t);
            }
        }
    }
}

#[test]
fn units_of_another_compiler_go_and_the_next_build_compiles_nothing() {
    let dir = TempDir::new("clean-real");
    new_package(&dir, "cleanme", "");
    build(dir.path());
    let debug = dir.join("target/debug");
    let stale = fake_old_unit(&debug, "cleanme", "00112233445566ff");
    // A file outside target/ with the same hash pattern: never touched.
    let outside = dir.write("notes/cleanme-00112233445566ff.d", "mine");
    let before = files_under(&dir.join("target"));

    // Dry run: says what it would do, removes nothing.
    let dry = giong(["clean", "--dry-run", "--json"])
        .dir(dir.path())
        .run();
    let dv = dry.report("clean");
    assert_eq!(dv["data"]["dry_run"], true);
    assert_eq!(
        files_under(&dir.join("target")),
        before,
        "--dry-run removed files"
    );
    let would = dv["data"]["bytes_freed"].as_u64().unwrap();
    assert!(would >= 4096, "{dv:#}");

    let out = giong(["clean", "--json"]).dir(dir.path()).run();
    let v = out.report("clean");
    assert_eq!(v["data"]["dry_run"], false);
    assert_eq!(
        v["data"]["bytes_freed"].as_u64().unwrap(),
        would,
        "dry run and real run disagree"
    );
    for p in &stale {
        assert!(!p.exists(), "{} was not removed", p.display());
    }
    assert!(outside.exists(), "a file outside target/ was removed");
    let t = &v["data"]["targets"][0];
    assert_eq!(
        PathBuf::from(t["target_dir"].as_str().unwrap()),
        dir.join("target"),
        "{v:#}"
    );
    assert_eq!(t["items"], stale.len(), "{v:#}");

    // The real unit stayed: cargo has nothing to compile.
    let log = build(dir.path());
    assert!(
        !log.contains("Compiling cleanme") && log.contains("Fresh cleanme"),
        "giong clean made the next build start again:\n{log}"
    );
}

#[test]
fn stale_incremental_data_goes_and_fresh_stays() {
    let dir = TempDir::new("clean-incr");
    new_package(&dir, "incr", "");
    build(dir.path());
    let inc = dir.join("target/debug/incremental");
    let fresh: Vec<PathBuf> = std::fs::read_dir(&inc)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert!(!fresh.is_empty());
    let old = inc.join("incr-oldsession");
    std::fs::create_dir_all(old.join("s-x")).unwrap();
    std::fs::write(old.join("s-x/dep-graph.bin"), vec![1u8; 1000]).unwrap();
    set_old(&old, 30);
    let v = giong(["clean", "--json"])
        .dir(dir.path())
        .run()
        .report("clean");
    assert!(!old.exists(), "30-day-old incremental data stayed: {v:#}");
    for f in fresh {
        assert!(
            f.exists(),
            "fresh incremental data {} was removed",
            f.display()
        );
    }
}

#[test]
fn only_tagged_directories_are_touched_when_scanning() {
    let root = TempDir::new("clean-scan");
    // A real project...
    let proj = root.join("a");
    std::fs::create_dir_all(&proj).unwrap();
    let proj_dir = TempDirAt(proj.clone());
    proj_dir.package();
    build(&proj);
    let debug = proj.join("target/debug");
    let stale = fake_old_unit(&debug, "scanned", "aabbccddeeff0011");
    // ...and a directory that looks like a build but has no cargo tag.
    let fake = root.join("b/target/debug/deps");
    std::fs::create_dir_all(&fake).unwrap();
    std::fs::write(fake.join("libx-aabbccddeeff0011.rlib"), "keep").unwrap();
    let fake_files = files_under(&root.join("b"));

    let v = giong(["clean", "--json"])
        .arg(root.path())
        .dir(root.path())
        .run()
        .report("clean");
    for p in &stale {
        assert!(!p.exists(), "{} was not removed: {v:#}", p.display());
    }
    assert_eq!(
        files_under(&root.join("b")),
        fake_files,
        "an untagged directory was touched"
    );
    assert_eq!(v["data"]["targets"].as_array().unwrap().len(), 1, "{v:#}");
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_target_is_removed_not_followed() {
    let dir = TempDir::new("clean-symlink");
    new_package(&dir, "linky", "");
    build(dir.path());
    let debug = dir.join("target/debug");
    let precious = dir.write("precious/data.txt", "do not delete");
    let stale = fake_old_unit(&debug, "linky", "0f0f0f0f0f0f0f0f");
    // A unit output that is a symlink to a directory outside target/.
    let link = stale_output_path(&debug, "linky", "0f0f0f0f0f0f0f0f", &stale);
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(precious.parent().unwrap(), &link).unwrap();
    let v = giong(["clean", "--json"])
        .dir(dir.path())
        .run()
        .report("clean");
    for p in &stale {
        assert!(!p.exists(), "{}: {v:#}", p.display());
    }
    assert!(
        std::fs::symlink_metadata(&link).is_err(),
        "the stale symlink stayed: {v:#}"
    );
    assert!(
        precious.exists(),
        "giong clean followed a symlink out of target/ and deleted {}",
        precious.display()
    );
}

#[test]
fn an_explicit_target_dir_is_respected() {
    let dir = TempDir::new("clean-ctd");
    let base = TempDir::new("clean-ctd-target");
    // Not created beforehand: cargo writes its CACHEDIR.TAG only into a
    // directory it creates (see the next test).
    let elsewhere = TempDirAt(base.join("t"));
    new_package(&dir, "ctd", "");
    let out = cargo()
        .args(["build", "--offline"])
        .env("CARGO_TARGET_DIR", elsewhere.path())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    let stale = fake_old_unit(&elsewhere.join("debug"), "ctd", "1234123412341234");
    let v = giong(["clean", "--json"])
        .env("CARGO_TARGET_DIR", elsewhere.path())
        .dir(dir.path())
        .run()
        .report("clean");
    for p in &stale {
        assert!(!p.exists(), "{}: {v:#}", p.display());
    }
}

/// A target directory made before the first build (a shared
/// `CARGO_TARGET_DIR` from a dotfile, a Docker volume) has no CACHEDIR.TAG:
/// cargo writes it only into a directory it creates. `giong clean` must not
/// answer "no build directory found" for the project's own build directory.
#[test]
fn a_target_dir_made_before_the_first_build_is_not_skipped_silently() {
    let dir = TempDir::new("clean-precreated");
    let elsewhere = TempDir::new("clean-precreated-target");
    new_package(&dir, "pre", "");
    let out = cargo()
        .args(["build", "--offline"])
        .env("CARGO_TARGET_DIR", elsewhere.path())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(
        !elsewhere.join("CACHEDIR.TAG").exists(),
        "cargo tagged it after all"
    );
    let v = giong(["clean", "--json"])
        .env("CARGO_TARGET_DIR", elsewhere.path())
        .dir(dir.path())
        .run()
        .report("clean");
    assert!(
        !v["data"]["targets"].as_array().unwrap().is_empty()
            || !v["findings"].as_array().unwrap().is_empty(),
        "the project's own build directory was skipped without a word: {v:#}"
    );
}

#[test]
fn all_removes_the_build_directory_and_nothing_else() {
    let dir = TempDir::new("clean-all");
    new_package(&dir, "everything", "");
    build(dir.path());
    let v = giong(["clean", "--all", "--json"])
        .dir(dir.path())
        .run()
        .report("clean");
    // Emptied, keeping only cargo's tag, so the directory stays recognised.
    let left: Vec<PathBuf> = files_under(&dir.join("target"));
    assert!(
        left.iter()
            .all(|p| p.file_name().is_some_and(|n| n == "CACHEDIR.TAG")),
        "--all left {left:?}: {v:#}"
    );
    assert!(dir.join("Cargo.toml").exists() && dir.join("src/main.rs").exists());
}

#[test]
fn no_build_directory_is_not_an_error() {
    let dir = TempDir::new("clean-none");
    new_package(&dir, "nothing", "");
    let out = giong(["clean", "--json"]).dir(dir.path()).run();
    let v = out.report("clean");
    assert_eq!(v["data"]["bytes_freed"], 0);
    out.expect_code(0);
}

#[test]
fn bad_days_are_usage_errors() {
    let dir = TempDir::new("clean-days");
    new_package(&dir, "days", "");
    build(dir.path());
    for d in ["-1", "abc", "1.5", "99999999999999999999"] {
        let out = giong(["clean", "--days", d, "--json"])
            .dir(dir.path())
            .run();
        assert_eq!(
            out.report("clean")["error"]["code"],
            "GIONG-E5000",
            "--days {d}"
        );
    }
}

/// A huge but valid number of days must neither panic (debug) nor wrap
/// around to a few hours (release) and remove what was built yesterday.
#[test]
fn huge_days_do_not_overflow() {
    let dir = TempDir::new("clean-days-huge");
    new_package(&dir, "huge", "");
    build(dir.path());
    let before = files_under(&dir.join("target"));
    for d in ["213503982334602", "18446744073709551615"] {
        let out = giong(["clean", "--days", d, "--json"])
            .dir(dir.path())
            .run();
        assert!(
            !out.stderr.contains("panicked at"),
            "--days {d}\n{}",
            out.dump()
        );
        out.report("clean");
    }
    assert_eq!(
        files_under(&dir.join("target")),
        before,
        "--days <huge> removed fresh units"
    );
}

/// `--days N` must look at when a unit was last rebuilt (the files cargo
/// rewrites), not at the unit directory's own time, which cargo leaves as it
/// was when the unit was first built.
#[test]
fn days_keeps_units_rebuilt_today() {
    let dir = TempDir::new("clean-days-today");
    new_package(&dir, "today", "");
    build(dir.path());
    for unit in unit_dirs(&dir.join("target/debug")) {
        // Only the directory: as if the unit was first built 60 days ago.
        let f = std::fs::File::open(unit).unwrap();
        let _ = f.set_modified(SystemTime::now() - Duration::from_secs(60 * 86_400));
    }
    dir.write(
        "src/main.rs",
        "fn main() {\n    println!(\"rebuilt\");\n}\n",
    );
    build(dir.path());
    giong(["clean", "--days", "7", "--json"])
        .dir(dir.path())
        .run()
        .report("clean");
    let log = build(dir.path());
    assert!(
        log.contains("Fresh today"),
        "giong clean --days 7 removed a unit rebuilt a moment ago:\n{log}"
    );
}

/// CACHEDIR.TAG's signature is a standard every cache uses (pytest, mypy,
/// ruff, borg, backup exclusions), not cargo's own. A directory that is not
/// a cargo build must never be removed, with or without `--all`.
#[test]
fn other_programs_cache_directories_are_never_removed() {
    let root = TempDir::new("clean-foreign-tag");
    let sig = "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by pytest.\n";
    root.write("py/.pytest_cache/CACHEDIR.TAG", sig);
    root.write("py/.pytest_cache/v/cache/lastfailed", "{}");
    root.write(
        "photos/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    root.write("photos/2026/holiday.jpg", "precious");
    let before = files_under(root.path());
    for args in [vec!["clean", "--all", "--json"], vec!["clean", "--json"]] {
        let v = giong(args.clone())
            .arg(root.path())
            .dir(root.path())
            .run()
            .report("clean");
        assert_eq!(
            files_under(root.path()),
            before,
            "giong {args:?} removed files: {v:#}"
        );
        assert_eq!(
            v["data"]["targets"].as_array().unwrap().len(),
            0,
            "{args:?}: {v:#}"
        );
    }
}

/// The same build directory named twice (or a directory and one inside it)
/// is one directory: the bytes freed are counted once.
#[test]
fn the_same_directory_named_twice_is_counted_once() {
    let dir = TempDir::new("clean-twice");
    new_package(&dir, "twice", "");
    build(dir.path());
    let once = giong(["clean", "--all", "--dry-run", "--json"])
        .arg(dir.path())
        .dir(dir.path())
        .run()
        .report("clean");
    let twice = giong(["clean", "--all", "--dry-run", "--json"])
        .arg(dir.path())
        .arg(dir.path())
        .arg(dir.join("target"))
        .dir(dir.path())
        .run()
        .report("clean");
    assert_eq!(
        once["data"]["bytes_freed"], twice["data"]["bytes_freed"],
        "{twice:#}"
    );
}

struct TempDirAt(PathBuf);

impl TempDirAt {
    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }

    fn package(&self) {
        std::fs::create_dir_all(self.0.join("src")).unwrap();
        std::fs::write(
            self.0.join("Cargo.toml"),
            "[package]\nname = \"scanned\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(self.0.join("src/main.rs"), "fn main() {}\n").unwrap();
    }
}
