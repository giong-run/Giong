//! What every command promises, checked for every command this build has
//! (read from `giong help --json`), so a command added by any branch is
//! covered the moment it is registered.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use crate::harness::{commands, giong, TempDir, VERSION};

/// Commands with a scenario file of their own in this directory. A command
/// missing here is still covered by the contract tests below; the coverage
/// test prints it so the scenario gets written.
const SCENARIOS: &[&str] = &[
    "doctor", "explain", "clean", "version", "help", "new", "dev",
];

/// How long a command may take outside a project before it is called hung:
/// with nothing to do it must answer at once.
const BLIND_TIMEOUT: Duration = Duration::from_secs(30);

fn listing(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            out.insert(p.strip_prefix(dir).unwrap().display().to_string());
            if p.is_dir() {
                stack.push(p);
            }
        }
    }
    out
}

/// A home of its own, so nothing a command writes under `~` lands in the
/// real one, while cargo and rustup still find their toolchains.
fn sandboxed(run: crate::harness::Run, home: &Path) -> crate::harness::Run {
    let real_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    let mut run = run
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_DATA_HOME", home.join(".local/share"));
    if let Some(h) = real_home {
        let cargo_home = std::env::var_os("CARGO_HOME").unwrap_or_else(|| h.join(".cargo").into());
        let rustup_home =
            std::env::var_os("RUSTUP_HOME").unwrap_or_else(|| h.join(".rustup").into());
        run = run
            .env("CARGO_HOME", cargo_home)
            .env("RUSTUP_HOME", rustup_home);
    }
    run
}

#[test]
fn help_lists_every_command_and_each_has_usage() {
    let cmds = commands();
    assert!(!cmds.is_empty());
    let unique: BTreeSet<_> = cmds.iter().collect();
    assert_eq!(
        unique.len(),
        cmds.len(),
        "a command is listed twice: {cmds:?}"
    );
    for c in &cmds {
        assert!(
            c.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
            "command names are lowercase words: {c:?}"
        );
        let text = giong([c.as_str(), "--help"]).run();
        text.expect_code(0);
        assert!(
            text.stdout.contains(&format!("giong {c}")),
            "giong {c} --help does not show its usage\n{}",
            text.dump()
        );
        for args in [
            vec![c.as_str(), "--help", "--json"],
            vec!["help", c.as_str(), "--json"],
        ] {
            let out = giong(args).run();
            let v = out.report("help");
            assert_eq!(v["data"]["name"], c.as_str(), "{}", out.dump());
            for f in v["data"]["flags"].as_array().expect("data.flags") {
                let name = f["name"].as_str().unwrap();
                assert!(
                    name.bytes()
                        .all(|b| b.is_ascii_lowercase() || b == b'-' || b.is_ascii_digit()),
                    "giong {c}: flag {name:?} is not a lowercase word"
                );
                assert!(f["takes_value"].is_boolean() && f["help"].is_string());
            }
        }
    }
}

#[test]
fn an_unknown_flag_is_a_coded_usage_error_on_every_command() {
    for c in commands() {
        let out = giong([c.as_str(), "--no-such-flag-e2e", "--json"]).run();
        let v = out.report(&c);
        assert_eq!(
            v["error"]["code"],
            "GIONG-E5000",
            "giong {c}\n{}",
            out.dump()
        );
        assert!(
            out.stderr.is_empty(),
            "giong {c} --json printed to stderr on a usage error\n{}",
            out.dump()
        );
        let text = giong([c.as_str(), "--no-such-flag-e2e"]).run();
        text.expect_code(2);
        assert!(
            text.stdout.is_empty() && text.stderr.starts_with("giong: GIONG-E5000"),
            "giong {c}: a usage error goes to stderr as `giong: GIONG-E5000 ...`\n{}",
            text.dump()
        );
        assert!(text.stderr.contains("Fix:"), "{}", text.dump());
    }
}

/// Every command, run with no words in an empty directory that is not a
/// project: it does its job or fails with a code, at once, writes nothing,
/// prints the same verdict as text and as JSON, and never panics.
#[test]
fn every_command_outside_a_project_answers_at_once_and_writes_nothing() {
    for c in commands() {
        let dir = TempDir::new(&format!("blind-{c}"));
        let home = TempDir::new(&format!("blind-home-{c}"));
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        let before = listing(&work);
        let out = sandboxed(giong([c.as_str(), "--json"]), home.path())
            .dir(&work)
            .timeout(BLIND_TIMEOUT)
            .run();
        let v = out.report(&c);
        assert!(
            !out.stderr.contains("panicked at"),
            "giong {c} panicked\n{}",
            out.dump()
        );
        assert_eq!(
            listing(&work),
            before,
            "giong {c} with no words wrote into the current directory"
        );
        // Every code it reports has an offline page.
        let mut codes: Vec<String> = v["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["code"].as_str().unwrap().to_string())
            .collect();
        if let Some(e) = v.get("error") {
            codes.push(e["code"].as_str().unwrap().to_string());
        }
        for code in codes {
            giong(["explain", code.as_str()]).run().expect_code(0);
        }
        let text = sandboxed(giong([c.as_str()]), home.path())
            .dir(&work)
            .timeout(BLIND_TIMEOUT)
            .run();
        assert!(!text.stderr.contains("panicked at"), "{}", text.dump());
        assert_eq!(
            text.code(),
            out.code(),
            "giong {c}: text and --json disagree on the exit code\n{}\n{}",
            text.dump(),
            out.dump()
        );
    }
}

#[test]
fn unknown_commands_are_coded_and_suggest_the_closest() {
    let out = giong(["frobnicate-e2e", "--json"]).run();
    let v = out.report("frobnicate-e2e");
    assert_eq!(v["error"]["code"], "GIONG-E5000");
    let out = giong(["doctr"]).run();
    out.expect_code(2);
    assert!(out.stderr.contains("giong doctor"), "{}", out.dump());
}

#[test]
fn bare_giong_version_and_json_placement() {
    let out = giong(Vec::<&str>::new()).run();
    out.expect_code(0);
    assert!(out.stdout.contains("Commands:"), "{}", out.dump());
    let v = giong(["version", "--json"]).run().report("version");
    assert_eq!(v["data"]["version"], VERSION);
    let v = giong(["--version", "--json"]).run().report("version");
    assert_eq!(v["data"]["version"], VERSION);
    let t = giong(["--version"]).run();
    t.expect_code(0);
    assert_eq!(t.stdout.trim(), format!("giong {VERSION}"));
    // A global flag before the command means the same as after it.
    let a = giong(["--json", "explain", "E5000"])
        .run()
        .report("explain");
    let b = giong(["explain", "E5000", "--json"])
        .run()
        .report("explain");
    assert_eq!(a, b);
    // After `--`, --json belongs to another program.
    let out = giong(["help", "--", "--json"]).run();
    out.expect_code(0);
    assert!(
        !out.stdout.trim_start().starts_with('{'),
        "--json after -- must not switch to JSON\n{}",
        out.dump()
    );
}

/// A path that is not UTF-8 is legal on Unix (and common in old Latin-1
/// home directories). The command must answer with a code, not a panic.
#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_is_not_a_panic() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = TempDir::new("non-utf8");
    let out = giong(["clean", "--dry-run", "--json"])
        .arg(OsStr::from_bytes(b"caf\xe9"))
        .dir(dir.path())
        .run();
    assert!(
        !out.stderr.contains("panicked at"),
        "giong panicked on a non-UTF-8 argument\n{}",
        out.dump()
    );
    // The command line cannot be read, so the document may name the
    // program instead of the command.
    let v = crate::schema::parse_single_document(&out.stdout).unwrap();
    let cmd = v["command"].as_str().unwrap_or("").to_string();
    assert!(cmd == "clean" || cmd == "giong", "{}", out.dump());
    out.report(&cmd);
    assert_eq!(v["error"]["code"], "GIONG-E5000", "{}", out.dump());
}

#[test]
fn coverage_of_commands_by_scenarios() {
    let missing: Vec<String> = commands()
        .into_iter()
        .filter(|c| !SCENARIOS.contains(&c.as_str()))
        .collect();
    if !missing.is_empty() {
        // Not a failure: the contract tests above already run these. The
        // tester thread adds a scenario file for each.
        eprintln!("NOTE: no scenario tests yet for: {}", missing.join(", "));
    }
}
