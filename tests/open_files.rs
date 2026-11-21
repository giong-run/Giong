//! A runtime raises the soft limit on open files to the hard one, as a
//! container started by containerd needs (1,024 soft, 524,288 hard), unless
//! told not to. Each test runs itself again in a child process, which lowers
//! its own soft limit first: the limit is process-wide, and raised once.

#![cfg(target_os = "linux")]

use std::process::Command;

const CHILD: &str = "GIONG_TEST_OPEN_FILES_CHILD";

fn limits() -> (u64, u64) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` fills the struct it is given.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) }, 0);
    (lim.rlim_cur, lim.rlim_max)
}

/// In the child: a soft limit of 256, below a hard limit above it.
fn lower_soft_limit() -> u64 {
    let (_, max) = limits();
    assert!(max > 256, "the hard limit is {max}");
    let lim = libc::rlimit {
        rlim_cur: 256,
        rlim_max: max,
    };
    // SAFETY: `setrlimit` reads the struct it is given.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) }, 0);
    max
}

fn run_child(name: &str) {
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn a_runtime_raises_the_soft_limit_on_open_files_to_the_hard_one() {
    if std::env::var_os(CHILD).is_none() {
        return run_child("a_runtime_raises_the_soft_limit_on_open_files_to_the_hard_one");
    }
    let max = lower_soft_limit();
    giong::block_on(async {});
    assert_eq!(limits(), (max, max));
}

#[test]
fn a_runtime_told_not_to_keeps_the_soft_limit() {
    if std::env::var_os(CHILD).is_none() {
        return run_child("a_runtime_told_not_to_keeps_the_soft_limit");
    }
    let max = lower_soft_limit();
    giong::Builder::new()
        .raise_open_files_limit(false)
        .block_on(async {})
        .unwrap();
    giong::Builder::new()
        .raise_open_files_limit(false)
        .workers(1)
        .build()
        .unwrap()
        .run(|_| async {})
        .unwrap();
    assert_eq!(limits(), (256, max));
}
