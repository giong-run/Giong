//! `giong test`: the project's tests, the way giong's own are run: once on the
//! driver the machine gives (io_uring on Linux when it is allowed), then again
//! on epoll, which containers under Docker's default seccomp profile get.
//! `.env` is read for the tests (development only); each failing test becomes
//! a finding with its panic message.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::Stdio;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use serde_json::json;

use super::Command;
use crate::args::{Args, Flag, Kind};
use crate::code::Code;
use crate::dotenv;
use crate::project::{cargo, cargo_missing, Project};
use crate::report::{Failure, Finding, Report, Severity};

pub const COMMAND: Command = Command {
    name: "test",
    summary: "run the tests on io_uring and again on epoll",
    usage: "giong test [--quick] [--nextest] [--timeout <secs>] [-- <cargo test flags>]\n\n\
            Runs cargo test twice on Linux: on the driver giong picks here (io_uring where it is \
            allowed), then with GIONG_DRIVER=epoll, the driver containers with Docker's default \
            seccomp profile get. Elsewhere it runs once. Variables in .env that the environment \
            does not set are given to the tests. Each failing test is reported with its panic \
            message. A run that takes longer than --timeout seconds (30 minutes by default) is \
            stopped and the tests still running are named. --quick runs once; --nextest uses cargo-nextest (it does not run \
            doctests). Flags after -- go to cargo: giong test -- -p api, giong test -- users.",
    flags: &[
        Flag {
            name: "quick",
            kind: Kind::Switch,
            help: "one run, on the default driver",
        },
        Flag {
            name: "nextest",
            kind: Kind::Switch,
            help: "run with cargo nextest instead of cargo test",
        },
        Flag {
            name: "timeout",
            kind: Kind::Value,
            help: "stop a run that takes longer than this many seconds (default 1800, 0: never)",
        },
    ],
    max_words: 0,
    run,
};

/// One run of the suite.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub passed: u64,
    pub failed: u64,
    pub ignored: u64,
    /// (test name, what it printed when it failed)
    pub failures: Vec<(String, String)>,
    /// The first compiler error, when the tests did not build.
    pub build_error: Option<String>,
    /// The run was stopped after this many seconds.
    pub timed_out: Option<u64>,
    /// Tests libtest said had been running for over 60 seconds.
    pub slow: Vec<String>,
}

/// Reads libtest's (or nextest's) output.
pub fn parse_output(out: &str) -> RunResult {
    let mut r = RunResult::default();
    let mut current: Option<(String, String)> = None;
    let mut sections: Vec<(String, String)> = Vec::new();
    for line in out.lines() {
        let t = line.trim();
        // libtest: "test users::create has been running for over 60 seconds"
        if let Some(name) = t
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" has been running for over 60 seconds"))
        {
            r.slow.push(name.to_string());
        }
        // libtest: "test result: FAILED. 3 passed; 1 failed; 0 ignored; ..."
        if let Some(rest) = t.strip_prefix("test result: ") {
            for part in rest.split(';') {
                let words: Vec<&str> = part.split_whitespace().collect();
                let n = |i: usize| words.get(i).and_then(|w| w.parse::<u64>().ok());
                for (i, w) in words.iter().enumerate() {
                    let count = if i > 0 { n(i - 1) } else { None };
                    match (*w, count) {
                        ("passed", Some(c)) => r.passed += c,
                        ("failed", Some(c)) => r.failed += c,
                        ("ignored", Some(c)) => r.ignored += c,
                        _ => {}
                    }
                }
            }
            continue;
        }
        // nextest: "Summary [   1.2s] 10 tests run: 9 passed, 1 failed, 0 skipped"
        if t.starts_with("Summary [") {
            for part in t.split([',', ':']) {
                let words: Vec<&str> = part.split_whitespace().collect();
                if let [count, what, ..] = words.as_slice() {
                    if let Ok(c) = count.parse::<u64>() {
                        match *what {
                            "passed" => r.passed += c,
                            "failed" => r.failed += c,
                            "skipped" => r.ignored += c,
                            _ => {}
                        }
                    }
                }
            }
            continue;
        }
        if r.build_error.is_none()
            && (t.starts_with("error[") || t.starts_with("error: could not compile"))
        {
            r.build_error = Some(t.to_string());
        }
        if let Some(name) = t
            .strip_prefix("---- ")
            .and_then(|s| s.strip_suffix(" stdout ----"))
        {
            if let Some(done) = current.take() {
                sections.push(done);
            }
            current = Some((name.to_string(), String::new()));
            continue;
        }
        if let Some((_, text)) = current.as_mut() {
            if t == "failures:" || t.starts_with("test result:") {
                sections.push(current.take().unwrap());
            } else {
                text.push_str(line);
                text.push('\n');
            }
            continue;
        }
        // "test users::create ... FAILED" (libtest) or "FAIL [ 0.1s] crate test" (nextest)
        let failed_name = t
            .strip_prefix("test ")
            .and_then(|s| s.strip_suffix(" ... FAILED"))
            .map(str::to_string)
            .or_else(|| {
                t.strip_prefix("FAIL [")
                    .and_then(|s| s.split_once(']'))
                    .map(|(_, rest)| rest.split_whitespace().last().unwrap_or("").to_string())
            });
        if let Some(name) = failed_name.filter(|n| !n.is_empty()) {
            if !r.failures.iter().any(|(n, _)| *n == name) {
                r.failures.push((name, String::new()));
            }
        }
    }
    if let Some(done) = current.take() {
        sections.push(done);
    }
    for (name, text) in sections {
        // The panic message, without the backtrace.
        let short: String = text
            .lines()
            .take_while(|l| !l.starts_with("stack backtrace:") && !l.starts_with("note: run with"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
        match r.failures.iter_mut().find(|(n, _)| *n == name) {
            Some(f) => f.1 = short,
            None => r.failures.push((name, short)),
        }
    }
    r
}

/// Whether a variable changes how cargo builds rather than what the tests
/// see: `.env` is the service's configuration, so these stay out of it.
fn is_build_setting(key: &str) -> bool {
    key.starts_with("CARGO")
        || key.starts_with("RUSTC")
        || key.starts_with("RUSTUP_")
        || matches!(key, "RUSTFLAGS" | "RUSTDOCFLAGS")
}

/// The `.env` variables the environment does not set already, and the names
/// of those left out because they are build settings.
fn env_from_dotenv(root: &Path) -> (Vec<(String, String)>, Vec<String>) {
    let Some((vars, _bad)) = dotenv::read(&root.join(".env")) else {
        return (Vec::new(), Vec::new());
    };
    let mut env = Vec::new();
    let mut build = Vec::new();
    for v in vars {
        if is_build_setting(&v.key) {
            build.push(v.key);
        } else if std::env::var_os(&v.key).is_none() {
            env.push((v.key, v.value));
        }
    }
    (env, build)
}

/// The unit tests build small projects with cargo while they run inside
/// `cargo test` themselves: the outer run's build settings (a sanitizer's
/// RUSTFLAGS, its target directory and target, its GIONG_DRIVER) are not the
/// inner project's.
#[cfg(test)]
pub(crate) fn without_outer_build(cmd: &mut std::process::Command) {
    for var in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTDOCFLAGS",
        "CARGO_ENCODED_RUSTDOCFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_TARGET_DIR",
        "TSAN_OPTIONS",
        "GIONG_DRIVER",
    ] {
        cmd.env_remove(var);
    }
}

/// Stops cargo and everything it started.
fn stop_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The group process_group(0) made: cargo, the test binary, what it spawned.
        let _ = std::process::Command::new("kill")
            .args(["-s", "KILL", "--", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
}

/// Runs the suite once; streams its output to stderr when `echo`.
fn run_once(
    root: &Path,
    driver: Option<&str>,
    nextest: bool,
    extra: &[String],
    env: &[(String, String)],
    echo: bool,
    timeout: Option<Duration>,
) -> Result<(RunResult, String, f64), Failure> {
    let mut cmd = cargo();
    if nextest {
        cmd.args(["nextest", "run", "--no-fail-fast"]);
    } else {
        cmd.args(["test", "--no-fail-fast"]);
    }
    cmd.args(extra)
        .current_dir(root)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(test)]
    without_outer_build(&mut cmd);
    if let Some(d) = driver {
        cmd.env("GIONG_DRIVER", d);
    }
    // Its own process group, so a timeout stops cargo and the test binary it runs.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(cargo_missing)?;
    // Both streams, in the order they come, read on two threads.
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);
    let mut all = String::new();
    let stderr = std::io::stderr();
    let mut timed_out = false;
    loop {
        let line = match timeout {
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(limit) => rx.recv_timeout(limit.saturating_sub(started.elapsed())),
        };
        match line {
            Ok(line) => {
                if echo {
                    let _ = writeln!(stderr.lock(), "{line}");
                }
                all.push_str(&line);
                all.push('\n');
            }
            Err(RecvTimeoutError::Timeout) => {
                timed_out = true;
                stop_tree(&mut child);
                break;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if !timed_out {
        for r in readers {
            let _ = r.join();
        }
    }
    let status = child.wait().map_err(cargo_missing)?;
    let mut result = parse_output(&all);
    if timed_out {
        result.timed_out = timeout.map(|t| t.as_secs());
        return Ok((result, all, started.elapsed().as_secs_f64()));
    }
    if !status.success()
        && result.failed == 0
        && result.failures.is_empty()
        && result.build_error.is_none()
    {
        result.build_error = Some(
            all.lines()
                .rev()
                .find(|l| l.trim_start().starts_with("error"))
                .unwrap_or("cargo test failed without a test failing")
                .trim()
                .to_string(),
        );
    }
    Ok((result, all, started.elapsed().as_secs_f64()))
}

fn run(args: &Args) -> Result<Report, Failure> {
    let project = Project::here()?;
    let nextest = args.has("nextest");
    if nextest {
        let ok = cargo()
            .args(["nextest", "--version"])
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            return Err(Failure::new(
                Code::NEXTEST_MISSING,
                "cargo-nextest is not installed",
                "cargo install cargo-nextest --locked, or leave out --nextest",
            ));
        }
    }
    let timeout = match args.number::<u64>("timeout")?.unwrap_or(1800) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    };
    let (env, build_settings) = env_from_dotenv(&project.root);
    let mut r = Report::new("test");
    r.data(
        "dotenv",
        json!(env.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()),
    );
    if !build_settings.is_empty() {
        r.push(
            Finding::new(
                Code::DOTENV_BUILD_SETTING,
                Severity::Warning,
                format!(".env sets {}: left out", build_settings.join(", ")),
            )
            .detail(
                ".env is the service's configuration; a cargo or rustc setting in it would change \
                 how the tests are built, and only for giong test.",
            )
            .fix("set it in the shell or in .cargo/config.toml, and delete it from .env"),
        );
    }
    let user_driver = std::env::var("GIONG_DRIVER").ok().filter(|d| !d.is_empty());
    let mut drivers: Vec<(String, Option<&str>)> = vec![(
        user_driver
            .clone()
            .map_or_else(|| "default".to_string(), |d| d.to_string()),
        None,
    )];
    let epoll_again =
        cfg!(target_os = "linux") && !args.has("quick") && user_driver.as_deref() != Some("epoll");
    if epoll_again {
        drivers.push(("epoll".into(), Some("epoll")));
    } else if !cfg!(target_os = "linux") && !args.has("quick") {
        r.push(
            Finding::new(
                Code::DRIVER_NOT_HERE,
                Severity::Info,
                "the tests ran once: the epoll driver is Linux only",
            )
            .detail("On Linux, giong test runs the suite again on epoll, which containers without io_uring get.")
            .fix("run giong test on Linux (or in CI) before a release"),
        );
    }
    let echo = !args.json();
    let mut runs = Vec::new();
    let mut failed_first: Vec<String> = Vec::new();
    for (index, (label, driver)) in drivers.into_iter().enumerate() {
        if echo {
            eprintln!("giong test: running on {label} driver");
        }
        let (result, _, secs) = run_once(
            &project.root,
            driver,
            nextest,
            &args.rest,
            &env,
            echo,
            timeout,
        )?;
        if let Some(limit) = result.timed_out {
            let still = if result.slow.is_empty() {
                "libtest named no test running for over 60 seconds".to_string()
            } else {
                format!("running for over 60 seconds: {}", result.slow.join(", "))
            };
            r.push(
                Finding::new(
                    Code::TEST_TIMEOUT,
                    Severity::Error,
                    format!("the {label} run took over {limit} s and was stopped"),
                )
                .detail(still)
                .fix(
                    "a test waits for something that never comes: give every wait a deadline \
                     (giong::time::timeout), or raise --timeout for a suite that is slow",
                ),
            );
        }
        if let Some(err) = &result.build_error {
            r.push(
                Finding::new(
                    Code::BUILD_FAILED,
                    Severity::Error,
                    "the tests do not build",
                )
                .detail(err.clone())
                .fix("giong check shows every error with its place"),
            );
        }
        for (name, text) in &result.failures {
            r.push(
                Finding::new(
                    Code::TEST_FAILED,
                    Severity::Error,
                    format!("{name} failed on the {label} driver"),
                )
                .detail(if text.is_empty() {
                    "it printed nothing".to_string()
                } else {
                    text.clone()
                })
                .fix(if index > 0 && !failed_first.contains(name) {
                    format!(
                        "it passes on the other driver: run GIONG_DRIVER=epoll cargo test {name}; \
                         a test that takes one recv() for one message is a common cause (TCP keeps \
                         no message boundaries)"
                    )
                } else {
                    format!("run cargo test {name} -- --nocapture to see all it printed")
                }),
            );
        }
        runs.push(json!({
            "driver": label,
            "passed": result.passed,
            "failed": result.failed,
            "ignored": result.ignored,
            "failures": result.failures.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            "seconds": (secs * 10.0).round() / 10.0,
            "built": result.build_error.is_none(),
            "timed_out": result.timed_out.is_some(),
        }));
        if result.build_error.is_some() || result.timed_out.is_some() {
            break; // It will not build (or finish) on the other driver either.
        }
        if index == 0 {
            failed_first = result.failures.iter().map(|(n, _)| n.clone()).collect();
        }
    }
    let total_failed: u64 = runs.iter().map(|x| x["failed"].as_u64().unwrap_or(0)).sum();
    for run in &runs {
        r.line(format!(
            "{} driver: {} passed, {} failed, {} ignored ({} s)",
            run["driver"].as_str().unwrap_or(""),
            run["passed"],
            run["failed"],
            run["ignored"],
            run["seconds"]
        ));
    }
    r.data("runs", json!(runs));
    r.data("failed", json!(total_failed));
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_build_settings_stay_out() {
        for key in [
            "RUSTFLAGS",
            "CARGO_TARGET_DIR",
            "RUSTC_WRAPPER",
            "RUSTUP_TOOLCHAIN",
        ] {
            assert!(is_build_setting(key), "{key}");
        }
        for key in ["DATABASE_URL", "RUST_LOG", "RUST_BACKTRACE"] {
            assert!(!is_build_setting(key), "{key}");
        }
    }

    #[test]
    fn libtest_output() {
        let out = "\
running 3 tests
test a ... ok
test users::create ... FAILED
test b ... ignored

failures:

---- users::create stdout ----

thread 'users::create' panicked at tests/x.rs:3:5:
assertion `left == right` failed
  left: 500
 right: 201
stack backtrace:
   0: something

failures:
    users::create

test result: FAILED. 1 passed; 1 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s

running 2 tests
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.0s
";
        let r = parse_output(out);
        assert_eq!((r.passed, r.failed, r.ignored), (3, 1, 1));
        assert_eq!(r.failures.len(), 1);
        assert_eq!(r.failures[0].0, "users::create");
        assert!(
            r.failures[0].1.contains("right: 201"),
            "{}",
            r.failures[0].1
        );
        assert!(!r.failures[0].1.contains("backtrace"));
        assert!(r.build_error.is_none());
    }

    #[test]
    fn nextest_output() {
        let out = "        PASS [   0.003s] api tests::a\n        FAIL [   0.010s] api tests::b\n     Summary [   0.2s] 3 tests run: 1 passed, 1 failed, 1 skipped\n";
        let r = parse_output(out);
        assert_eq!((r.passed, r.failed, r.ignored), (1, 1, 1));
        assert_eq!(r.failures[0].0, "tests::b");
    }

    #[test]
    fn build_errors() {
        let r = parse_output("   Compiling x\nerror[E0425]: cannot find value `y`\n --> src/lib.rs:1:1\nerror: could not compile `x`\n");
        assert_eq!(
            r.build_error.as_deref(),
            Some("error[E0425]: cannot find value `y`")
        );
    }

    /// A real project with one test that passes and one that fails only on
    /// epoll: the failure is reported for that driver alone.
    #[test]
    fn runs_a_real_suite() {
        let dir = std::env::temp_dir().join(format!("giong-test-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"suite\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "#[test]\nfn fine() {}\n#[test]\nfn needs_url() { assert_eq!(std::env::var(\"FROM_DOTENV\").as_deref(), Ok(\"yes\")); }\n\
             #[test]\nfn not_on_epoll() { assert_ne!(std::env::var(\"GIONG_DRIVER\").as_deref(), Ok(\"epoll\"), \"epoll!\"); }\n",
        )
        .unwrap();
        std::fs::write(dir.join(".env"), "FROM_DOTENV=yes\n").unwrap();
        let (env, _) = env_from_dotenv(&dir);
        let (first, _, _) = run_once(&dir, None, false, &[], &env, false, None).unwrap();
        assert_eq!((first.passed, first.failed), (3, 0), "{first:?}");
        let (second, _, _) = run_once(&dir, Some("epoll"), false, &[], &env, false, None).unwrap();
        assert_eq!((second.passed, second.failed), (2, 1));
        assert_eq!(second.failures[0].0, "not_on_epoll");
        assert!(second.failures[0].1.contains("epoll!"));
        std::fs::write(dir.join("src/lib.rs"), "fn broken( {}\n").unwrap();
        let (broken, _, _) = run_once(&dir, None, false, &[], &env, false, None).unwrap();
        assert!(broken.build_error.is_some(), "{broken:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A test that never ends: the run stops at the limit, cargo and the
    /// test binary with it.
    #[test]
    fn stops_a_hung_suite() {
        let dir = std::env::temp_dir().join(format!("giong-test-hang-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"hang\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/lib.rs"),
            "#[test]\nfn forever() { std::thread::sleep(std::time::Duration::from_secs(600)); }\n",
        )
        .unwrap();
        // Build first, so the limit measures the test, not the compiler.
        let mut build = cargo();
        build.args(["test", "--no-run", "-q"]).current_dir(&dir);
        without_outer_build(&mut build);
        assert!(build.status().unwrap().success());
        let started = Instant::now();
        let (r, _, _) = run_once(
            &dir,
            None,
            false,
            &[],
            &[],
            false,
            Some(Duration::from_secs(3)),
        )
        .unwrap();
        assert_eq!(r.timed_out, Some(3));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "{:?}",
            started.elapsed()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
