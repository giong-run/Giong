//! Ctrl+C and Ctrl+Break on Windows, what `SIGINT` and `SIGTERM` are on
//! Unix (those are in `tests/multi.rs`).
#![cfg(windows)]

use std::io::BufRead;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_BREAK_EVENT};
use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

/// The exit code of a process that Ctrl+C or Ctrl+Break ended.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Ctrl+Break to the process group of `pid`, which shares this console.
fn ctrl_break(pid: u32) {
    // SAFETY: a plain call; the group is the child's own.
    let ok = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    assert_ne!(
        ok,
        0,
        "GenerateConsoleCtrlEvent: {}",
        std::io::Error::last_os_error()
    );
}

#[test]
fn a_second_ctrl_break_ends_a_process_whose_shutdown_hangs() {
    // The handler took every console event: once a shutdown had been asked
    // for, a main future that never returned kept the process running, and
    // Ctrl+C could no longer stop it.
    if std::env::var_os("GIONG_CONSOLE_CHILD_HANG").is_some() {
        giong::Builder::new()
            .workers(2)
            .shutdown_on_signals(true)
            .build()
            .unwrap()
            .run(|index| async move {
                if index == 0 {
                    println!("ready");
                    giong::shutdown_signal().await;
                    println!("shutdown asked");
                }
                // Never answers the shutdown.
                std::future::pending::<()>().await;
            })
            .unwrap();
        println!("clean exit");
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "a_second_ctrl_break_ends_a_process_whose_shutdown_hangs",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GIONG_CONSOLE_CHILD_HANG", "1")
        .creation_flags(CREATE_NEW_PROCESS_GROUP)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        lines.by_ref().any(|l| l.unwrap().contains("ready")),
        "child never became ready"
    );
    ctrl_break(child.id());
    assert!(
        lines
            .by_ref()
            .any(|l| l.unwrap().contains("shutdown asked")),
        "the first Ctrl+Break did not ask for a shutdown"
    );
    // The first event only asks: the process is still running.
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the first Ctrl+Break ended the process"
    );
    ctrl_break(child.id());
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let rest: Vec<String> = lines.map(|l| l.unwrap()).collect();
    let status = status.unwrap_or_else(|| panic!("the second Ctrl+Break was ignored: {rest:?}"));
    assert_eq!(
        status.code().map(|c| c as u32),
        Some(STATUS_CONTROL_C_EXIT),
        "{status:?}, output: {rest:?}"
    );
}
