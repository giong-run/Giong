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

/// The soft limit a runtime raises to: the hard limit, or `fs.nr_open`, the
/// most Linux allows, when the hard limit is unlimited.
fn raised(max: u64) -> u64 {
    if max != libc::RLIM_INFINITY {
        return max;
    }
    std::fs::read_to_string("/proc/sys/fs/nr_open")
        .unwrap()
        .trim()
        .parse()
        .unwrap()
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
    assert_eq!(limits(), (raised(max), max));
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

/// What `sh -c 'ulimit -n'` prints when started with `giong::process`.
async fn child_soft_limit(cmd: &mut giong::process::Command) -> String {
    let out = cmd.output().await.unwrap();
    assert!(out.status.success(), "{out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

/// A child gets the soft limit the process had before the runtime raised
/// it (issue #372): a child that uses `select(2)` needs descriptors below
/// 1,024. A limit the caller's own `pre_exec` sets is kept, and spawning a
/// command twice adds nothing twice.
#[test]
fn a_child_process_gets_the_soft_limit_from_before_it_was_raised() {
    if std::env::var_os(CHILD).is_none() {
        return run_child("a_child_process_gets_the_soft_limit_from_before_it_was_raised");
    }
    let max = lower_soft_limit();
    giong::block_on(async move {
        assert_eq!(limits(), (raised(max), max));
        let mut sh = giong::process::Command::new("sh");
        sh.args(["-c", "ulimit -n"]);
        assert_eq!(child_soft_limit(&mut sh).await, "256");
        assert_eq!(child_soft_limit(&mut sh).await, "256");

        let mut own = giong::process::Command::new("sh");
        own.args(["-c", "ulimit -n"]);
        // SAFETY: only `setrlimit`, async-signal-safe, between fork and exec.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(own.as_std_mut(), move || {
                let lim = libc::rlimit {
                    rlim_cur: 300,
                    rlim_max: max,
                };
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim);
                Ok(())
            });
        }
        assert_eq!(child_soft_limit(&mut own).await, "300");
    });
    // The process itself keeps the raised limit.
    assert_eq!(limits(), (raised(max), max));
}
