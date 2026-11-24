//! `giong dev --background`, `status`, `logs` and `stop`: the words an agent
//! uses to run a service it cannot keep a terminal open for. What matters:
//! `--background` returns (it does not hold the caller's output open), the
//! state it reports is true, `stop` stops what giong started and nothing
//! else, and a stale state file from a killed runner never makes giong signal
//! a process that merely got the same pid.
//!
//! The service here is a std-only binary, so a build takes about a second;
//! every test points `GIONG_HOME` at its own directory, so `--all` sees only
//! what the test started.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::harness::*;

/// A project whose binary serves "dev-ctl-ok" over HTTP on `E2E_PORT`,
/// printing a line per request, so `logs` has something to show.
const SERVICE: &str = r#"use std::io::{Read, Write};
fn main() {
    let port = std::env::var("E2E_PORT").expect("E2E_PORT");
    let l = std::net::TcpListener::bind(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    println!("dev-ctl service listening on {port}");
    for s in l.incoming() {
        let Ok(mut s) = s else { continue };
        let mut buf = [0u8; 1024];
        let _ = s.read(&mut buf);
        println!("dev-ctl served a request");
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\ndev-ctl-ok");
    }
}
"#;

fn project(tag: &str) -> TempDir {
    let dir = TempDir::new(tag);
    new_package(&dir, "devctl", "");
    dir.write("src/main.rs", SERVICE);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    dir
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// `giong dev <words>` in the project, isolated from this machine's runners.
fn dev(dir: &TempDir, words: &[&str], port: u16) -> Run {
    let mut run = giong(["dev"]);
    for w in words {
        run = run.arg(w);
    }
    run.arg("--json")
        .dir(dir.path())
        .env("GIONG_HOME", dir.join("home"))
        .env("E2E_PORT", port.to_string())
        .env_remove("LISTEN")
        .env_remove("CARGO_TARGET_DIR")
        .timeout(Duration::from_secs(300))
}

fn answers(port: u16) -> bool {
    let Ok(mut s) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(500))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = s.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.contains("dev-ctl-ok")
}

fn wait_for(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let start = Instant::now();
    while !ok() {
        assert!(start.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stops whatever a test started, even when it fails half-way.
struct StopOnDrop<'a>(&'a TempDir);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        let _ = dev(self.0, &["stop"], 0)
            .timeout(Duration::from_secs(90))
            .run();
    }
}

/// Whether `pid` runs. A zombie has ended: in a container whose init does
/// not reap, a detached runner that exited stays one.
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        let state = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next());
        if state == Some("Z") {
            return false;
        }
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn with_nothing_running_status_logs_and_stop_say_so_and_succeed() {
    if !has_command("dev") {
        return;
    }
    let dir = project("devctl-idle");
    for words in [
        &["status"][..],
        &["logs"],
        &["stop"],
        &["status", "--all"],
        &["stop", "--all"],
    ] {
        let out = dev(&dir, words, 0).run();
        let v = out.report("dev");
        out.expect_code(0);
        if words == ["status"] {
            assert_eq!(v["data"]["running"], false, "{}", out.dump());
            assert_eq!(v["data"]["state"], "stopped", "{}", out.dump());
        }
        if words == ["status", "--all"] {
            assert_eq!(
                v["data"]["projects"],
                serde_json::json!([]),
                "{}",
                out.dump()
            );
        }
    }
    // Nothing was started, so nothing was written into the project either.
    assert!(!dir.join(".giong/dev.json").exists());
}

#[test]
fn outside_a_project_the_words_are_a_coded_failure() {
    if !has_command("dev") {
        return;
    }
    let dir = TempDir::new("devctl-nowhere");
    std::fs::create_dir_all(dir.join("home")).unwrap();
    for words in [&["status"][..], &["logs"], &["stop"]] {
        let out = dev(&dir, words, 0).run();
        let v = out.report("dev");
        out.expect_code(2);
        assert!(v["error"]["code"].is_string(), "{}", out.dump());
    }
}

#[test]
fn background_and_once_do_not_go_together() {
    if !has_command("dev") {
        return;
    }
    let dir = project("devctl-once");
    let out = dev(&dir, &["--background", "--once"], 0).run();
    let v = out.report("dev");
    out.expect_code(2);
    assert_eq!(v["error"]["code"], "GIONG-E5000", "{}", out.dump());
    assert!(
        !dir.join(".giong/dev.json").exists(),
        "a runner was started"
    );
}

/// The whole life of a background runner, the way an agent drives it.
#[test]
#[cfg_attr(
    windows,
    ignore = "known bug: on Windows the background runner inherits the caller's output pipe, so a piped caller never sees --background end"
)]
fn background_serves_reports_logs_and_stops() {
    if !has_command("dev") || !toolchain("background_serves_reports_logs_and_stops") {
        return;
    }
    let dir = project("devctl-life");
    let _stop = StopOnDrop(&dir);
    let port = free_port();

    // It returns once the service serves, and does not keep our pipes open:
    // the harness would wait until its timeout otherwise.
    let started = Instant::now();
    let out = dev(&dir, &["--background"], port).run();
    let v = out.report("dev");
    out.expect_code(0);
    assert!(
        started.elapsed() < Duration::from_secs(240),
        "{}",
        out.dump()
    );
    assert_eq!(v["data"]["running"], true, "{}", out.dump());
    assert_eq!(v["data"]["background"], true, "{}", out.dump());
    wait_for("the service to answer", Duration::from_secs(30), || {
        answers(port)
    });

    let out = dev(&dir, &["status"], port).run();
    let v = out.report("dev");
    out.expect_code(0);
    assert_eq!(v["data"]["state"], "serving", "{}", out.dump());
    let runner = v["data"]["pid"].as_u64().expect("the runner's pid") as u32;
    #[cfg(unix)]
    let service = v["data"]["service_pid"]
        .as_u64()
        .expect("the service's pid") as u32;

    let out = dev(&dir, &["status", "--all"], port).run();
    let v = out.report("dev");
    let projects = v["data"]["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1, "{}", out.dump());

    // A second start does not start a second runner.
    let out = dev(&dir, &["--background"], port).run();
    let v = out.report("dev");
    assert_eq!(
        v["data"]["pid"].as_u64(),
        Some(u64::from(runner)),
        "{}",
        out.dump()
    );

    wait_for("the request in the log", Duration::from_secs(10), || {
        dev(&dir, &["logs", "--lines", "20"], port)
            .run()
            .stdout
            .contains("dev-ctl served a request")
    });

    let out = dev(&dir, &["stop"], port).run();
    let v = out.report("dev");
    out.expect_code(0);
    assert_eq!(
        v["data"]["stopped"][0]["pid"].as_u64(),
        Some(u64::from(runner)),
        "{}",
        out.dump()
    );
    assert!(!answers(port), "the service still answers after stop");
    #[cfg(unix)]
    {
        assert!(!alive(runner), "the runner {runner} still runs after stop");
        wait_for("the service to end", Duration::from_secs(10), || {
            !alive(service)
        });
    }
    let out = dev(&dir, &["status"], port).run();
    assert_eq!(
        out.report("dev")["data"]["state"],
        "stopped",
        "{}",
        out.dump()
    );
    let out = dev(&dir, &["status", "--all"], port).run();
    assert_eq!(
        out.report("dev")["data"]["projects"],
        serde_json::json!([]),
        "{}",
        out.dump()
    );
}

/// A first build that fails: `--background` returns with the compiler's
/// error, coded, and the runner stays to build again on the next save.
#[test]
#[cfg_attr(
    windows,
    ignore = "known bug: on Windows the background runner inherits the caller's output pipe, so a piped caller never sees --background end"
)]
fn background_with_a_broken_build_returns_the_error() {
    if !has_command("dev") || !toolchain("background_with_a_broken_build_returns_the_error") {
        return;
    }
    let dir = project("devctl-broken");
    let _stop = StopOnDrop(&dir);
    dir.write("src/main.rs", "fn main() { let x: u32 = \"no\"; }\n");
    let port = free_port();
    let out = dev(&dir, &["--background"], port).run();
    let v = out.report("dev");
    assert_eq!(out.code(), 1, "{}", out.dump());
    let codes = v["findings"].to_string();
    assert!(codes.contains("GIONG-E"), "{}", out.dump());
    assert!(
        out.stdout.contains("mismatched types") || out.stdout.contains("E0308"),
        "{}",
        out.dump()
    );

    // Fixing the file is enough: the runner builds and serves.
    dir.write("src/main.rs", SERVICE);
    wait_for(
        "the fixed build to answer",
        Duration::from_secs(240),
        || answers(port),
    );
}

/// A runner killed with SIGKILL (or a reboot) leaves `.giong/dev.json`
/// behind. Its pid can belong to any other process of this user later; giong
/// must not report it as giong dev, refuse to start because of it, or signal
/// it on `stop`.
/// The start time the way giong dev records it (Linux: field 22 of
/// /proc/<pid>/stat; elsewhere `ps -o lstart=`).
#[cfg(unix)]
fn started(pid: u32) -> Option<String> {
    if cfg!(target_os = "linux") {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        return rest.split_whitespace().nth(19).map(String::from);
    }
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|t| !t.is_empty())
}

#[cfg(unix)]
#[test]
fn a_stale_state_file_never_touches_a_process_that_reused_the_pid() {
    if !has_command("dev") {
        return;
    }
    let spawn = || -> Child {
        Command::new("sleep")
            .arg("300")
            .stdin(Stdio::null())
            .spawn()
            .unwrap()
    };
    struct Reap(Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let (a, b) = (Reap(spawn()), Reap(spawn()));
    let (pa, pb) = (a.0.id(), b.0.id());
    // First as an old giong wrote it (no start times), then with the start
    // times of the two sleeps, so only the program and parent checks stand
    // between `stop` and them.
    for with_times in [false, true] {
        let dir = project("devctl-stale");
        let (ta, tb) = if with_times {
            (started(pa), started(pb))
        } else {
            (None, None)
        };
        // As a runner that was building writes it: the shortest wait in `stop`.
        let exe = std::fs::canonicalize("/bin/sleep").unwrap_or_else(|_| "/bin/sleep".into());
        dir.write(
            ".giong/dev.json",
            &serde_json::json!({
                "pid": pa, "service_pid": pb, "background": true, "exe": exe,
                "pid_started": ta, "service_started": tb,
                "started_at": 1_700_000_000_000u64, "addresses": ["127.0.0.1:1"],
                "build": {"state": "building"}
            })
            .to_string(),
        );
        std::fs::create_dir_all(dir.join("home/run")).unwrap();
        std::fs::write(
            dir.join(format!("home/run/{pa}.json")),
            serde_json::json!({"pid": pa, "root": dir.path().display().to_string(),
                               "started_at": 1_700_000_000_000u64})
            .to_string(),
        )
        .unwrap();

        let out = dev(&dir, &["status"], 0).run();
        let v = out.report("dev");
        assert_eq!(
            v["data"]["running"],
            false,
            "a `sleep` that got the pid is reported as giong dev (start times: {with_times}): {}",
            out.dump()
        );
        for words in [&["stop"][..], &["stop", "--all"]] {
            let out = dev(&dir, words, 0).timeout(Duration::from_secs(90)).run();
            out.report("dev");
            for p in [pa, pb] {
                assert!(
                    alive(p),
                    "`giong dev {words:?}` signalled pid {p}, a sleep that reused the pid \
                     (start times: {with_times})"
                );
            }
        }
    }
    drop((a, b));
}
