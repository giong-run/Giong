//! Child processes, pipes, and the process's own stdin and stdout.
//!
//! Linux only: `giong::process` is Linux only for now (pidfds).

#![cfg(target_os = "linux")]

use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use giong::error::{self, Code};
use giong::process::{self, Command};
use giong::signal::SignalKind;
use giong::time::{sleep, timeout};

/// `true` while a process with this ID exists (a zombie counts).
fn exists(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

async fn gone(pid: u32) -> bool {
    for _ in 0..200 {
        if !exists(pid) {
            return true;
        }
        sleep(Duration::from_millis(10)).await;
    }
    false
}

#[test]
fn output_collects_both_streams_and_the_exit_code() {
    giong::block_on(async {
        let out = Command::new("sh")
            .args(["-c", "echo out; echo err >&2; exit 3"])
            .output()
            .await
            .unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
    });
}

#[test]
fn status_reports_success_and_failure() {
    giong::block_on(async {
        assert!(Command::new("true").status().await.unwrap().success());
        assert_eq!(
            Command::new("false").status().await.unwrap().code(),
            Some(1)
        );
    });
}

#[test]
fn a_missing_program_fails_to_spawn() {
    giong::block_on(async {
        let err = Command::new("/nonexistent/giong-test").spawn().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    });
}

#[test]
fn a_megabyte_through_cat_both_ways_at_once() {
    giong::block_on(async {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let data: Vec<u8> = (0..1 << 20).map(|i| (i * 7 % 251) as u8).collect();
        let expected = data.clone();
        // Write and read at once: cat blocks when its output pipe is full.
        let writer = giong::spawn(async move {
            stdin.write_all(data).await.0.unwrap();
            // Dropping stdin closes it: cat sees end of input.
        });
        let mut got = Vec::new();
        stdout.read_to_end(&mut got).await.unwrap();
        writer.await.unwrap();
        assert_eq!(got.len(), expected.len());
        assert!(got == expected);
        assert!(child.wait().await.unwrap().success());
    });
}

#[test]
fn kill_and_signal_reach_the_child() {
    giong::block_on(async {
        let mut child = Command::new("sleep").arg("100").spawn().unwrap();
        assert!(child.try_wait().unwrap().is_none());
        child.kill().unwrap();
        let status = child.wait().await.unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        // Waiting again returns the same status, and signals are no-ops.
        assert_eq!(child.wait().await.unwrap(), status);
        child.signal(SignalKind::TERMINATE).unwrap();

        let mut child = Command::new("sleep").arg("100").spawn().unwrap();
        child.signal(SignalKind::TERMINATE).unwrap();
        assert_eq!(child.wait().await.unwrap().signal(), Some(libc::SIGTERM));
    });
}

#[test]
fn a_dropped_wait_loses_nothing() {
    giong::block_on(async {
        let mut child = Command::new("sh")
            .args(["-c", "sleep 0.2; exit 7"])
            .spawn()
            .unwrap();
        for _ in 0..5 {
            assert!(timeout(Duration::from_millis(10), child.wait())
                .await
                .is_err());
        }
        assert_eq!(child.wait().await.unwrap().code(), Some(7));
        sleep(Duration::from_millis(20)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn a_dropped_child_is_reaped_and_kill_on_drop_kills() {
    giong::block_on(async {
        // Runs on after the drop, then is reaped: no zombie left.
        let child = Command::new("sh")
            .args(["-c", "sleep 0.1"])
            .spawn()
            .unwrap();
        let pid = child.id();
        drop(child);
        assert!(exists(pid));
        assert!(gone(pid).await, "the dropped child was not reaped");

        let child = Command::new("sleep")
            .arg("100")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id();
        drop(child);
        assert!(gone(pid).await, "kill_on_drop did not kill the child");
    });
}

#[test]
fn a_pipe_end_can_be_a_childs_stdout() {
    giong::block_on(async {
        let (reader, writer) = process::pipe().unwrap();
        let mut child = Command::new("echo")
            .arg("through a pipe")
            .stdout(writer)
            .spawn()
            .unwrap();
        let mut got = Vec::new();
        reader.read_to_end(&mut got).await.unwrap();
        assert_eq!(got, b"through a pipe\n");
        assert!(child.wait().await.unwrap().success());
    });
}

#[test]
fn a_pipe_carries_bytes_and_reports_end_of_file() {
    giong::block_on(async {
        let (reader, writer) = process::pipe().unwrap();
        writer.write_all(&b"abc"[..]).await.0.unwrap();
        let (res, buf) = reader.read(Vec::with_capacity(8)).await;
        assert_eq!(res.unwrap(), 3);
        assert_eq!(buf, b"abc");
        drop(writer);
        let (res, _) = reader.read(Vec::with_capacity(8)).await;
        assert_eq!(res.unwrap(), 0);

        let (reader, writer) = process::pipe().unwrap();
        drop(reader);
        // The close goes through the ring with the next batch.
        sleep(Duration::from_millis(5)).await;
        let (res, _) = writer.write(&b"x"[..]).await;
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    });
}

#[test]
fn a_dropped_pipe_read_leaves_nothing_behind() {
    giong::block_on(async {
        let (reader, writer) = process::pipe().unwrap();
        for _ in 0..50 {
            let res = timeout(
                Duration::from_millis(1),
                reader.read(Vec::with_capacity(64)),
            )
            .await;
            assert!(res.is_err());
        }
        writer.write_all(&b"after"[..]).await.0.unwrap();
        let (res, buf) = reader.read(Vec::with_capacity(64)).await;
        assert_eq!(res.unwrap(), 5);
        assert_eq!(buf, b"after");
        sleep(Duration::from_millis(20)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn spawn_outside_a_runtime_panics_with_a_code() {
    let res = std::panic::catch_unwind(|| Command::new("true").spawn());
    let panic = res.unwrap_err();
    let msg = panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap();
    assert!(msg.contains("GIONG-E0011"), "{msg}");
}

#[test]
fn codes_exist_for_children_and_signals() {
    // The codes this module raises are documented (tests/error_docs.rs
    // checks the pages); here, that the not-allowed signals are refused.
    let err = giong::signal::listen(SignalKind::KILL).unwrap_err();
    assert_eq!(error::code(&err), Some(Code::SignalNotAllowed));
}

/// Run by `own_stdin_and_stdout_on_the_worker` in a child copy of this test
/// binary: echoes its stdin to its stdout, upper-cased, between markers.
#[test]
fn stdio_echo_child() {
    if std::env::var_os("GIONG_STDIO_CHILD").is_none() {
        return;
    }
    giong::block_on(async {
        let input = process::stdin().unwrap();
        let output = process::stdout().unwrap();
        let mut all = Vec::new();
        input.read_to_end(&mut all).await.unwrap();
        let mut reply = b"<<".to_vec();
        reply.extend(all.to_ascii_uppercase());
        reply.extend_from_slice(b">>");
        output.write_all(reply).await.0.unwrap();
    });
}

#[test]
fn own_stdin_and_stdout_on_the_worker() {
    giong::block_on(async {
        let exe = std::env::current_exe().unwrap();
        let mut child = Command::new(exe)
            .args(["--exact", "stdio_echo_child", "--test-threads=1", "--quiet"])
            .env("GIONG_STDIO_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        stdin.write_all(&b"hello over stdio"[..]).await.0.unwrap();
        drop(stdin);
        let out = child.wait_with_output().await.unwrap();
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("<<HELLO OVER STDIO>>"), "{text}");
    });
}

#[test]
fn own_stdin_from_a_regular_file_is_read_from_its_position() {
    giong::block_on(async {
        let path = std::env::temp_dir().join(format!("giong-stdin-{}.txt", std::process::id()));
        std::fs::write(&path, b"from a file").unwrap();
        let exe = std::env::current_exe().unwrap();
        let out = Command::new(exe)
            .args(["--exact", "stdio_echo_child", "--test-threads=1", "--quiet"])
            .env("GIONG_STDIO_CHILD", "1")
            .stdin(std::fs::File::open(&path).unwrap())
            .output()
            .await
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(out.status.success(), "{out:?}");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("<<FROM A FILE>>"), "{text}");
    });
}
