//! `giong doctor` against broken machines and projects: every case must be
//! caught with the right code, a fix, and the right exit status, and the
//! edits it offers must leave a project cargo still reads.

use std::time::Duration;

use serde_json::Value;

use crate::harness::{fake_tool, giong, new_package, TempDir};

fn codes(v: &Value) -> Vec<String> {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap().to_string())
        .collect()
}

fn finding<'a>(v: &'a Value, code: &str) -> &'a Value {
    v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == code)
        .unwrap_or_else(|| panic!("no {code} finding in {v:#}"))
}

/// Applies a finding's edits to the files under `root`, from the last byte
/// to the first so earlier offsets stay right.
fn apply_edits(root: &std::path::Path, f: &Value) {
    let mut edits: Vec<&Value> = f["edits"].as_array().unwrap().iter().collect();
    edits.sort_by_key(|e| std::cmp::Reverse(e["start_byte"].as_u64().unwrap()));
    for e in edits {
        let path = root.join(e["file"].as_str().unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        let s = e["start_byte"].as_u64().unwrap() as usize;
        let en = e["end_byte"].as_u64().unwrap() as usize;
        assert!(en <= text.len(), "edit past the end of {}", path.display());
        assert!(
            text.is_char_boundary(s) && text.is_char_boundary(en),
            "edit splits a character in {}",
            path.display()
        );
        let new = format!(
            "{}{}{}",
            &text[..s],
            e["replacement"].as_str().unwrap(),
            &text[en..]
        );
        std::fs::write(&path, new).unwrap();
    }
}

fn cargo_reads(root: &std::path::Path) {
    let out = std::process::Command::new(std::env::var_os("CARGO").unwrap_or("cargo".into()))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "cargo cannot read the project after giong's edit:\n{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(root.join("Cargo.toml")).unwrap()
    );
}

#[test]
fn a_healthy_package_has_no_error_and_reports_its_facts() {
    let dir = TempDir::new("doctor-ok");
    new_package(
        &dir,
        "ok",
        "\n[profile.dev]\ndebug = \"line-tables-only\"\n",
    );
    let out = giong(["doctor", "--json"]).dir(dir.path()).run();
    let v = out.report("doctor");
    for (k, kind) in [("os", "string"), ("arch", "string"), ("driver", "string")] {
        assert!(v["data"][k].is_string(), "data.{k} is not a {kind}: {v:#}");
    }
    assert!(v["data"]["cpus"].as_u64().unwrap() >= 1);
    assert_eq!(
        v["data"]["project"].as_str().map(std::path::PathBuf::from),
        Some(dir.path().to_path_buf()),
        "{v:#}"
    );
    assert!(!codes(&v).contains(&"GIONG-E5012".to_string()), "{v:#}");
    let want_driver = if cfg!(target_os = "linux") {
        &["io_uring", "epoll"][..]
    } else if cfg!(target_os = "macos") {
        &["kqueue"]
    } else {
        &["iocp"]
    };
    assert!(
        want_driver.contains(&v["data"]["driver"].as_str().unwrap()),
        "{v:#}"
    );
}

#[test]
fn rustc_missing_is_an_error_with_a_fix() {
    let dir = TempDir::new("doctor-norustc");
    let out = giong(["doctor", "--machine", "--json"])
        .dir(dir.path())
        .env("RUSTC", dir.join("no-such-rustc"))
        .run();
    let v = out.report("doctor");
    let f = finding(&v, "GIONG-E5002");
    assert_eq!(f["severity"], "error");
    assert!(f["fix"].as_str().unwrap().contains("rustup"), "{f:#}");
    out.expect_code(1);
}

#[test]
fn an_old_rustc_is_caught() {
    let dir = TempDir::new("doctor-oldrustc");
    let rustc = fake_tool(
        &dir.join("bin"),
        "rustc",
        "echo 'rustc 1.70.0 (90c541806 2023-05-31)'",
        "echo rustc 1.70.0 (90c541806 2023-05-31)",
    );
    let out = giong(["doctor", "--machine", "--json"])
        .dir(dir.path())
        .env("RUSTC", &rustc)
        .run();
    let v = out.report("doctor");
    let f = finding(&v, "GIONG-E5003");
    assert_eq!(f["severity"], "error");
    assert!(f["title"].as_str().unwrap().contains("1.70"), "{f:#}");
}

#[test]
fn nightly_and_odd_version_strings_are_read() {
    for (unix, win) in [
        (
            "echo 'rustc 1.99.0-nightly (abcdef012 2026-10-01)'",
            "echo rustc 1.99.0-nightly (abcdef012 2026-10-01)",
        ),
        ("echo 'rustc 1.85.0'", "echo rustc 1.85.0"),
        ("echo 'garbage that is not a version'", "echo garbage"),
        ("printf '\\377\\376 not utf8\\n'", "echo ÿþ"),
        ("exit 3", "exit /b 3"),
    ] {
        let dir = TempDir::new("doctor-oddrustc");
        let rustc = fake_tool(&dir.join("bin"), "rustc", unix, win);
        let out = giong(["doctor", "--machine", "--json"])
            .dir(dir.path())
            .env("RUSTC", &rustc)
            .run();
        assert!(!out.stderr.contains("panicked at"), "{}", out.dump());
        let v = out.report("doctor");
        assert!(
            !codes(&v).contains(&"GIONG-E5003".to_string()) || unix.contains("1.70"),
            "{unix}: {v:#}"
        );
    }
}

/// A toolchain that hangs (a rustup proxy waiting on a lock, a network
/// mount) must not hang `giong doctor`: agents run it unattended.
#[test]
#[ignore = "known bug: host::output has no timeout (review comment on the CLI skeleton PR)"]
fn a_hanging_rustc_does_not_hang_doctor() {
    let dir = TempDir::new("doctor-hang");
    let rustc = fake_tool(
        &dir.join("bin"),
        "rustc",
        "sleep 120",
        "ping -n 121 127.0.0.1 > nul",
    );
    let out = giong(["doctor", "--machine", "--json"])
        .dir(dir.path())
        .env("RUSTC", &rustc)
        .timeout(Duration::from_secs(30))
        .run();
    let v = out.report("doctor");
    assert!(!v["findings"].as_array().unwrap().is_empty(), "{v:#}");
}

#[cfg(unix)]
#[test]
fn no_c_linker_is_caught() {
    let dir = TempDir::new("doctor-nocc");
    let bin = dir.join("bin");
    fake_tool(&bin, "cc", "exit 127", "");
    // `cc` exists but cannot run: remove the execute bit.
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(bin.join("cc"), std::fs::Permissions::from_mode(0o644)).unwrap();
    // A PATH with only our directory and the toolchain, so the system cc is
    // not found.
    let rustc = std::process::Command::new("rustc")
        .arg("--print")
        .arg("sysroot")
        .output()
        .ok()
        .map(|o| std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()).join("bin"));
    let mut paths = vec![bin.clone()];
    paths.extend(rustc);
    let out = giong(["doctor", "--machine", "--json"])
        .dir(dir.path())
        .env("PATH", std::env::join_paths(paths).unwrap())
        .run();
    let v = out.report("doctor");
    let f = finding(&v, "GIONG-E5021");
    assert_eq!(f["severity"], "error");
}

#[cfg(unix)]
#[test]
fn a_low_open_files_limit_is_caught() {
    let dir = TempDir::new("doctor-nofile");
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "ulimit -Sn 256 && ulimit -Hn 256 && exec '{}' doctor --machine --json",
            crate::harness::GIONG
        ))
        .current_dir(dir.path())
        .output()
        .unwrap();
    let v = crate::schema::parse_single_document(&String::from_utf8_lossy(&out.stdout)).unwrap();
    crate::schema::check_report(&v, "doctor").unwrap();
    let f = finding(&v, "GIONG-E5013");
    assert!(f["title"].as_str().unwrap().contains("256"), "{f:#}");
    assert_eq!(v["data"]["open_files_limit"]["soft"], 256);
}

#[test]
fn tokio_runtime_crates_are_found_at_their_line() {
    let dir = TempDir::new("doctor-tokio");
    new_package(
        &dir,
        "uses-tokio",
        "\n[dependencies]\n# tokio would be a comment here\nserde = \"1\"\nreqwest = \"0.12\"\n\
         tokio = { version = \"1\", features = [\"sync\"] }\n\n\
         [dev-dependencies]\nhyper = \"1\"\n",
    );
    let out = giong(["doctor", "--json"]).dir(dir.path()).run();
    let v = out.report("doctor");
    let f = finding(&v, "GIONG-E5004");
    assert!(f["title"].as_str().unwrap().contains("reqwest"), "{f:#}");
    assert_eq!(f["file"], "Cargo.toml");
    assert_eq!(f["line"], 11, "points at the reqwest line: {f:#}");
    let titles: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "GIONG-E5004")
        .map(|f| f["title"].as_str().unwrap())
        .collect();
    assert_eq!(
        titles.len(),
        1,
        "tokio with only `sync` and dev-dependencies are fine: {titles:?}"
    );
}

#[test]
#[ignore = "known bug: the finding points at line 1, not the renamed dependency (review comment on the CLI skeleton PR)"]
fn a_renamed_tokio_dependency_is_still_tokio() {
    let dir = TempDir::new("doctor-tokio-renamed");
    new_package(
        &dir,
        "renamed",
        "\n[dependencies]\nrt = { package = \"tokio\", version = \"1\", features = [\"full\"] }\n",
    );
    let v = giong(["doctor", "--json"])
        .dir(dir.path())
        .run()
        .report("doctor");
    let f = finding(&v, "GIONG-E5004");
    assert_eq!(f["line"], 9, "{f:#}");
}

/// The fast-profile edit, applied to every shape of Cargo.toml, gives a file
/// cargo reads, and doctor is then satisfied.
#[test]
fn the_dev_profile_edit_applies_cleanly_in_every_shape() {
    check_profile_shapes(&[
        ("none", ""),
        ("no-newline", "\n[dependencies]"),
        ("debug-true", "\n[profile.dev]\ndebug = true\n"),
        (
            "debug-2-comment",
            "\n[profile.dev]\nopt-level = 0\ndebug = 2 # full\n\n[profile.release]\nlto = true\n",
        ),
        ("full-string", "\n[profile.dev]\ndebug = \"full\"\n"),
        ("header-only", "\n[profile.dev]\n"),
        (
            "header-then-table",
            "\n[profile.dev]\n[profile.release]\ndebug = true\n",
        ),
        ("crlf", "\r\n[profile.dev]\r\ndebug = true\r\n"),
        ("crlf-none", "\r\n[dependencies]\r\n"),
        (
            "unicode",
            "\n# Gióng — dịch vụ\n[profile.dev]\ndebug = true\n",
        ),
        ("no-newline-at-end", "\n[profile.dev]\ndebug = true"),
    ]);
}

/// Headers TOML allows but the line scanner does not recognise: the edit
/// adds a second `[profile.dev]` and cargo refuses the file.
#[test]
#[ignore = "known bug: dev_profile_finding adds a duplicate [profile.dev] (review comment on the CLI skeleton PR)"]
fn the_dev_profile_edit_handles_every_header_spelling() {
    check_profile_shapes(&[
        ("header-comment", "\n[profile.dev] # fast\ndebug = true\n"),
        ("header-spaces", "\n[ profile.dev ]\ndebug=true\n"),
        ("inline-table", "\n[profile]\ndev = { debug = true }\n"),
    ]);
}

fn check_profile_shapes(shapes: &[(&str, &str)]) {
    for (name, extra) in shapes {
        let dir = TempDir::new(&format!("doctor-profile-{name}"));
        new_package(&dir, "p", extra);
        let v = giong(["doctor", "--json"])
            .dir(dir.path())
            .run()
            .report("doctor");
        let Some(f) = v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["code"] == "GIONG-E5012")
        else {
            panic!("{name}: no GIONG-E5012 for a profile with full debug info: {v:#}");
        };
        assert_eq!(f["fixable"], true, "{name}");
        apply_edits(dir.path(), f);
        cargo_reads(dir.path());
        let again = giong(["doctor", "--json"])
            .dir(dir.path())
            .run()
            .report("doctor");
        assert!(
            !codes(&again).contains(&"GIONG-E5012".to_string()),
            "{name}: after the edit doctor still reports GIONG-E5012:\n{}\n{again:#}",
            dir.read("Cargo.toml")
        );
    }
}

#[test]
fn env_example_keys_missing_from_env_are_listed() {
    let dir = TempDir::new("doctor-env");
    new_package(&dir, "envy", "");
    dir.write(
        ".env.example",
        "DATABASE_URL=postgres://localhost/app\nexport REDIS_URL=redis://127.0.0.1\n# COMMENTED=1\nGIONG_E2E_SET_IN_ENV=\n",
    );
    dir.write(".env", "DATABASE_URL=postgres://u@h/db\n");
    let v = giong(["doctor", "--json"])
        .dir(dir.path())
        .env("GIONG_E2E_SET_IN_ENV", "1")
        .run()
        .report("doctor");
    let missing: Vec<&str> = v["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "GIONG-E5016")
        .map(|f| f["title"].as_str().unwrap())
        .collect();
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert!(missing[0].contains("REDIS_URL"), "{missing:?}");
}

/// A project whose Cargo.toml cargo cannot read is the first thing an agent
/// needs to hear about; doctor must not report "everything is here".
#[test]
#[ignore = "known bug: doctor drops the project checks silently when cargo metadata fails (review comment on the CLI skeleton PR)"]
fn a_broken_manifest_is_reported() {
    let dir = TempDir::new("doctor-broken-manifest");
    dir.write("Cargo.toml", "[package]\nname = \"broken\"\nversion = \n");
    let out = giong(["doctor", "--json"]).dir(dir.path()).run();
    let v = out.report("doctor");
    assert!(
        codes(&v)
            .iter()
            .any(|c| c == "GIONG-E5002" || c == "GIONG-E5001"),
        "doctor is silent about a Cargo.toml cargo cannot read: {v:#}"
    );
}

#[test]
fn machine_only_skips_the_project() {
    let dir = TempDir::new("doctor-machine");
    new_package(&dir, "m", "\n[dependencies]\nreqwest = \"0.12\"\n");
    let v = giong(["doctor", "--machine", "--json"])
        .dir(dir.path())
        .run()
        .report("doctor");
    assert!(!codes(&v).contains(&"GIONG-E5004".to_string()), "{v:#}");
}
