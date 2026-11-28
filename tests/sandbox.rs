//! The sandbox (`Builder::sandbox`). The kernel keeps a thread's io_uring
//! restrictions for its life, so every case runs in a copy of this test
//! binary of its own: the parent test starts it with `GIONG_SANDBOX_CASE`
//! naming the case, and the child runs only that case.
//!
//! Which layers apply depends on the kernel: the ring restrictions on every
//! kernel with io_uring, the per-task restrictions and open filters on
//! Linux 7.0. Each case checks what the kernel it runs on can do, as
//! `giong::sandbox::report()` describes it, and the kernel's answers to a
//! ring of its own where the layers are there.

#![cfg(target_os = "linux")]

use std::process::Command;

use giong::error::{self, Code};
use giong::sandbox::{FileAccess, Layer, LayerState, Mode, Report, Sandbox};

const CASE: &str = "GIONG_SANDBOX_CASE";

/// Runs `case` in a child process; returns its stderr. Fails the test if
/// the child failed.
fn run_case(case: &str, env: &[(&str, &str)]) -> String {
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CASE, case)
        .env_remove("GIONG_SANDBOX")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "case {case} failed:\n{stdout}\n{stderr}"
    );
    stderr
}

/// The child: runs the case its parent named.
#[test]
fn child() {
    let Ok(case) = std::env::var(CASE) else {
        return;
    };
    match case.as_str() {
        "none" => case_none(),
        "read_only" => case_read_only(),
        "read_write" => case_read_write(),
        "require" => case_require(),
        "audit" => case_audit(),
        "conflict" => case_conflict(),
        "epoll" => case_epoll(),
        "run" => case_run(),
        "env" => case_env(),
        other => panic!("unknown case {other}"),
    }
}

/// Whether this kernel has per-task restrictions, as the report says.
/// With `GIONG_TEST_EXPECT_SANDBOX=complete` (a run on Linux 7.0 or newer
/// sets it), a kernel without them fails the test instead.
fn has_task_layer(report: &Report) -> bool {
    let has = report.layer(Layer::TaskRestrictions).unwrap().state == LayerState::Applied;
    if expect_complete() {
        assert!(has, "GIONG_TEST_EXPECT_SANDBOX=complete, but: {report:?}");
    }
    has
}

fn expect_complete() -> bool {
    std::env::var("GIONG_TEST_EXPECT_SANDBOX").as_deref() == Ok("complete")
}

fn on_io_uring() -> bool {
    giong::driver_kind() == giong::DriverKind::IoUring
}

/// A temporary directory with one file in it, `a.txt`.
fn dir_with_file(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("giong-sandbox-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), b"hello").unwrap();
    dir
}

/// A TCP round trip through the runtime, which every sandbox allows.
async fn echo_works() {
    let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = giong::spawn(async move {
        let conn = listener.accept().await.unwrap();
        let data = conn.recv().await.unwrap().to_vec();
        conn.write_all(data).await.0.unwrap();
    });
    let client = giong::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(&b"ping"[..]).await.0.unwrap();
    let mut got = Vec::new();
    while got.len() < 4 {
        let data = client.recv().await.unwrap();
        assert!(!data.is_empty());
        got.extend_from_slice(&data);
    }
    assert_eq!(got, b"ping");
    server.await.unwrap();
}

/// What the kernel does with operations submitted to a new ring of this
/// thread, as a compromised task would submit them: `(open for reading,
/// open for writing, unlink)` results, or `None` where io_uring cannot be
/// set up at all.
fn raw_ring(path: &std::path::Path) -> Option<(i32, i32, i32)> {
    use io_uring::{opcode, types, IoUring};
    use std::os::unix::ffi::OsStrExt;
    let mut ring = IoUring::new(8).ok()?;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    let cwd = types::Fd(libc::AT_FDCWD);
    let entries = [
        opcode::OpenAt::new(cwd, c.as_ptr())
            .flags(libc::O_RDONLY | libc::O_CLOEXEC)
            .build(),
        opcode::OpenAt::new(cwd, c.as_ptr())
            .flags(libc::O_WRONLY | libc::O_CLOEXEC)
            .build(),
        opcode::UnlinkAt::new(cwd, c.as_ptr()).build(),
    ];
    let mut results = Vec::new();
    for sqe in entries {
        // SAFETY: the path outlives the submission, waited for here.
        unsafe { ring.submission().push(&sqe).unwrap() };
        ring.submit_and_wait(1).unwrap();
        let res = ring.completion().next().unwrap().result();
        if res >= 0 && results.len() < 2 {
            // SAFETY: the open returned a descriptor we own.
            unsafe { libc::close(res) };
        }
        results.push(res);
    }
    Some((results[0], results[1], results[2]))
}

fn no_new_privs() -> bool {
    // SAFETY: plain prctl query.
    unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) == 1 }
}

fn case_none() {
    let dir = dir_with_file("none");
    giong::Builder::new()
        .sandbox(Sandbox::best_effort())
        .block_on(async {
            let report = giong::sandbox::report().expect("a sandbox");
            assert_eq!(report.mode, Mode::BestEffort);
            assert_eq!(report.files, FileAccess::None);
            if on_io_uring() {
                let ring = report.layer(Layer::RingRestrictions).unwrap();
                assert_eq!(ring.state, LayerState::Applied, "{report:?}");
            }
            assert_eq!(
                report.layer(Layer::OpenFilters).unwrap().state,
                LayerState::NotNeeded
            );
            // Opening by path is refused by giong itself, with the rule.
            let e = giong::fs::read(dir.join("a.txt")).await.unwrap_err();
            assert_eq!(error::code(&e), Some(Code::SandboxDenied), "{e}");
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied);
            assert!(e.to_string().contains("FileAccess::ReadOnly"), "{e}");
            let e = giong::fs::metadata(&dir).await.unwrap_err();
            assert_eq!(error::code(&e), Some(Code::SandboxDenied), "{e}");
            let e = giong::fs::remove_file(dir.join("a.txt")).await.unwrap_err();
            assert_eq!(error::code(&e), Some(Code::SandboxDenied), "{e}");
            assert!(dir.join("a.txt").exists());
            // The network is untouched.
            echo_works().await;
            if has_task_layer(&report) {
                assert!(no_new_privs());
                // A ring this thread makes now is restricted as well.
                let (read, write, unlink) = raw_ring(&dir.join("a.txt")).unwrap();
                assert_eq!(
                    (read, write, unlink),
                    (-libc::EACCES, -libc::EACCES, -libc::EACCES)
                );
            } else {
                eprintln!("note: no per-task restrictions on this kernel: {report:?}");
            }
        })
        .unwrap();
}

fn case_read_only() {
    let dir = dir_with_file("ro");
    giong::Builder::new()
        .sandbox(Sandbox::best_effort().files(FileAccess::ReadOnly))
        .block_on(async {
            let report = giong::sandbox::report().expect("a sandbox");
            assert_eq!(giong::fs::read(dir.join("a.txt")).await.unwrap(), b"hello");
            assert!(giong::fs::metadata(dir.join("a.txt"))
                .await
                .unwrap()
                .is_file());
            let root = giong::fs::Root::new(&dir).await.unwrap();
            assert_eq!(root.read("a.txt").await.unwrap(), b"hello");
            for e in [
                giong::fs::write(dir.join("b.txt"), &b"x"[..])
                    .await
                    .unwrap_err(),
                giong::fs::remove_file(dir.join("a.txt")).await.unwrap_err(),
                giong::fs::create_dir(dir.join("d")).await.unwrap_err(),
                giong::fs::rename(dir.join("a.txt"), dir.join("c.txt"))
                    .await
                    .unwrap_err(),
                root.create("e.txt").await.unwrap_err(),
            ] {
                assert_eq!(error::code(&e), Some(Code::SandboxDenied), "{e}");
            }
            assert!(!dir.join("b.txt").exists());
            if has_task_layer(&report) {
                let filters = report.layer(Layer::OpenFilters).unwrap();
                if expect_complete() {
                    assert!(report.is_complete(), "{report:?}");
                }
                if filters.state == LayerState::Applied {
                    // The kernel lets a new ring open for reading, and
                    // refuses an open for writing and an unlink.
                    let (read, write, unlink) = raw_ring(&dir.join("a.txt")).unwrap();
                    assert!(read >= 0, "open for reading: {read}");
                    assert_eq!((write, unlink), (-libc::EACCES, -libc::EACCES));
                } else {
                    eprintln!("note: per-task restrictions without open filters: {report:?}");
                }
            }
        })
        .unwrap();
}

fn case_read_write() {
    let dir = dir_with_file("rw");
    giong::Builder::new()
        .sandbox(Sandbox::best_effort().files(FileAccess::ReadWrite))
        .block_on(async {
            giong::fs::write(dir.join("b.txt"), &b"x"[..])
                .await
                .unwrap();
            giong::fs::rename(dir.join("b.txt"), dir.join("c.txt"))
                .await
                .unwrap();
            giong::fs::create_dir(dir.join("d")).await.unwrap();
            giong::fs::remove_dir(dir.join("d")).await.unwrap();
            giong::fs::remove_file(dir.join("c.txt")).await.unwrap();
            let file = giong::fs::File::open(dir.join("a.txt")).await.unwrap();
            assert_eq!(file.metadata().await.unwrap().len(), 5);
            file.sync_all().await.unwrap();
            echo_works().await;
        })
        .unwrap();
}

fn case_require() {
    let probe = giong::Builder::new()
        .sandbox(Sandbox::require())
        .block_on(async { giong::sandbox::report().unwrap() });
    match probe {
        Ok(report) => {
            // Only a kernel with every layer gets here, or a process where
            // io_uring is blocked (Docker's seccomp profile), which needs
            // none.
            assert!(report.is_complete(), "{report:?}");
            let ring = report.layer(Layer::RingRestrictions).unwrap().state;
            let task = report.layer(Layer::TaskRestrictions).unwrap().state;
            assert!(
                task == LayerState::Applied
                    || (ring, task) == (LayerState::NotNeeded, LayerState::NotNeeded),
                "{report:?}"
            );
        }
        Err(e) => {
            assert!(!expect_complete(), "{e}");
            assert_eq!(error::code(&e), Some(Code::SandboxUnavailable), "{e}");
            assert!(e.to_string().contains("TaskRestrictions"), "{e}");
        }
    }
}

fn case_audit() {
    let dir = dir_with_file("audit");
    let nnp_before = no_new_privs();
    giong::Builder::new()
        .sandbox(Sandbox::audit())
        .block_on(async {
            let report = giong::sandbox::report().unwrap();
            assert!(report.layers.iter().all(|l| l.state == LayerState::Audit));
            // Nothing is refused; each case is logged once.
            for _ in 0..3 {
                assert_eq!(giong::fs::read(dir.join("a.txt")).await.unwrap(), b"hello");
            }
            giong::fs::write(dir.join("b.txt"), &b"x"[..])
                .await
                .unwrap();
            assert_eq!(no_new_privs(), nnp_before);
        })
        .unwrap();
}

fn case_conflict() {
    giong::Builder::new()
        .sandbox(Sandbox::best_effort())
        .block_on(async {})
        .unwrap();
    // Another thread, another sandbox: the thread layer cannot differ.
    std::thread::spawn(|| {
        let e = giong::Builder::new()
            .sandbox(Sandbox::require().files(FileAccess::ReadWrite))
            .block_on(async {})
            .unwrap_err();
        assert_eq!(error::code(&e), Some(Code::SandboxConflict), "{e}");
        let report = giong::Builder::new()
            .sandbox(Sandbox::best_effort().files(FileAccess::ReadWrite))
            .block_on(async { giong::sandbox::report().unwrap() })
            .unwrap();
        assert_eq!(
            report.layer(Layer::TaskRestrictions).unwrap().state,
            LayerState::Unavailable
        );
    })
    .join()
    .unwrap();
}

fn case_epoll() {
    let dir = dir_with_file("epoll");
    giong::Builder::new()
        .driver(giong::DriverKind::Epoll)
        .sandbox(Sandbox::best_effort())
        .block_on(async {
            let report = giong::sandbox::report().unwrap();
            assert_eq!(
                report.layer(Layer::RingRestrictions).unwrap().state,
                LayerState::NotNeeded
            );
            let e = giong::fs::read(dir.join("a.txt")).await.unwrap_err();
            assert_eq!(error::code(&e), Some(Code::SandboxDenied), "{e}");
            echo_works().await;
        })
        .unwrap();
}

/// A multi-worker runtime: every worker gets the sandbox, and so do the
/// calling thread and a blocking thread.
fn case_run() {
    let dir = dir_with_file("run");
    // `scripts/no-io-uring.py` sets it before the test starts.
    let nnp_before = no_new_privs();
    let file = dir.join("a.txt");
    giong::Builder::new()
        .workers(2)
        .sandbox(Sandbox::best_effort().files(FileAccess::ReadOnly))
        .build()
        .unwrap()
        .run(move |_| {
            let file = file.clone();
            async move {
                let report = giong::sandbox::report().unwrap();
                assert_eq!(giong::fs::read(&file).await.unwrap(), b"hello");
                let e = giong::fs::remove_file(&file).await.unwrap_err();
                assert_eq!(error::code(&e), Some(Code::SandboxDenied), "{e}");
                let task_layer = has_task_layer(&report);
                let nnp = giong::spawn_blocking(no_new_privs).await.unwrap();
                assert_eq!(nnp, task_layer || nnp_before);
            }
        })
        .unwrap();
    // The calling thread too, where the kernel has the layer.
    if no_new_privs() && !nnp_before {
        let (_, write, _) = raw_ring(&dir.join("a.txt")).unwrap();
        assert_eq!(write, -libc::EACCES);
    }
}

/// `GIONG_SANDBOX` gives a runtime without a sandbox of its own one.
fn case_env() {
    giong::block_on(async {
        let report = giong::sandbox::report().expect("a sandbox from GIONG_SANDBOX");
        assert_eq!(report.mode, Mode::BestEffort);
        assert_eq!(report.files, FileAccess::ReadOnly);
    });
}

#[test]
fn sandbox_without_file_access_refuses_paths_and_keeps_the_network() {
    run_case("none", &[]);
}

#[test]
fn read_only_sandbox_reads_and_refuses_changes() {
    run_case("read_only", &[]);
}

#[test]
fn read_write_sandbox_allows_every_file_operation() {
    run_case("read_write", &[]);
}

#[test]
fn require_starts_only_with_every_layer() {
    run_case("require", &[]);
}

#[test]
fn audit_mode_refuses_nothing_and_logs_each_case_once() {
    let stderr = run_case("audit", &[]);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("{\"giong\":\"sandbox-audit\""))
        .collect();
    assert_eq!(lines.len(), 2, "{stderr}");
    assert!(lines[0].contains("\"op\":\"open\""), "{}", lines[0]);
    assert!(lines[0].contains("a.txt"), "{}", lines[0]);
    assert!(
        lines[1].contains("\"op\":\"open for writing\""),
        "{}",
        lines[1]
    );
    assert!(lines[1].contains("\"needs\":\"ReadWrite\""), "{}", lines[1]);
}

#[test]
fn a_second_different_sandbox_in_one_process_is_refused() {
    run_case("conflict", &[]);
}

#[test]
fn sandbox_on_the_epoll_driver_still_refuses_paths() {
    run_case("epoll", &[]);
}

#[test]
fn every_thread_of_a_running_runtime_is_covered() {
    run_case("run", &[]);
}

#[test]
fn giong_sandbox_gives_a_runtime_a_sandbox() {
    run_case("env", &[("GIONG_SANDBOX", "read-only")]);
}

#[test]
fn a_bad_giong_sandbox_value_is_refused() {
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args(["--exact", "child", "--nocapture", "--test-threads=1"])
        .env(CASE, "env")
        .env("GIONG_SANDBOX", "everything")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("InvalidConfig") && stderr.contains("GIONG_SANDBOX"),
        "{stderr}"
    );
}
