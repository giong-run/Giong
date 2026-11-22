//! Signals as streams. In a test binary of their own: handlers are
//! process-wide.

use std::time::Duration;

use giong::error::{self, Code};
use giong::signal::{self, SignalKind};
use giong::time::{sleep, timeout};

/// Sends `signal` to a short-lived thread of its own. A signal sent to the
/// whole process may land on a worker waiting in `kevent` or `io_uring_enter`,
/// and ThreadSanitizer holds it back until that thread next calls a function
/// it intercepts, which a waiting worker never does. `raise` and the thread's
/// exit are intercepted, so the handler runs before `join` returns.
fn raise(signal: i32) {
    std::thread::spawn(move || {
        // SAFETY: sends a signal to this thread; the process has a handler.
        assert_eq!(unsafe { libc::raise(signal) }, 0);
    })
    .join()
    .unwrap();
}

#[test]
fn every_listener_sees_a_delivery() {
    giong::block_on(async {
        let mut a = signal::listen(SignalKind::USER1).unwrap();
        let mut b = signal::listen(SignalKind::USER1).unwrap();
        assert_eq!(a.kind(), SignalKind::USER1);
        raise(libc::SIGUSR1);
        timeout(Duration::from_secs(5), a.recv()).await.unwrap();
        timeout(Duration::from_secs(5), b.recv()).await.unwrap();
        // Nothing more is pending.
        assert!(timeout(Duration::from_millis(50), a.recv()).await.is_err());
    });
}

#[test]
fn deliveries_while_nobody_waits_are_kept_and_coalesce() {
    giong::block_on(async {
        let mut s = signal::listen(SignalKind::USER2).unwrap();
        raise(libc::SIGUSR2);
        raise(libc::SIGUSR2);
        sleep(Duration::from_millis(50)).await;
        timeout(Duration::from_secs(5), s.recv()).await.unwrap();
        assert!(timeout(Duration::from_millis(50), s.recv()).await.is_err());
    });
}

#[test]
fn a_listener_on_another_thread_is_woken() {
    let mut s = signal::listen(SignalKind::WINDOW_CHANGE).unwrap();
    let waiter = std::thread::spawn(move || {
        giong::block_on(async move {
            timeout(Duration::from_secs(5), s.recv()).await.unwrap();
        })
    });
    std::thread::sleep(Duration::from_millis(50));
    raise(libc::SIGWINCH);
    waiter.join().unwrap();
}

#[test]
fn a_handled_signal_no_longer_ends_the_process() {
    giong::block_on(async {
        let mut s = signal::listen(SignalKind::HANGUP).unwrap();
        drop(signal::listen(SignalKind::HANGUP).unwrap());
        raise(libc::SIGHUP);
        timeout(Duration::from_secs(5), s.recv()).await.unwrap();
        drop(s);
        // Still alive after a delivery with no listener at all.
        raise(libc::SIGHUP);
        sleep(Duration::from_millis(20)).await;
    });
}

#[test]
#[cfg(target_os = "linux")]
fn real_time_signals_work() {
    giong::block_on(async {
        let rt = libc::SIGRTMIN() + 3;
        let mut s = signal::listen(SignalKind::from_raw(rt)).unwrap();
        raise(rt);
        timeout(Duration::from_secs(5), s.recv()).await.unwrap();
    });
}

#[test]
fn signals_that_cannot_be_listened_for_have_a_code() {
    for signal in [
        libc::SIGKILL,
        libc::SIGSTOP,
        libc::SIGSEGV,
        libc::SIGBUS,
        libc::SIGILL,
        libc::SIGFPE,
        0,
        -1,
        65,
        32,
    ] {
        let err = signal::listen(SignalKind::from_raw(signal)).unwrap_err();
        assert_eq!(error::code(&err), Some(Code::SignalNotAllowed), "{signal}");
    }
}

#[test]
#[cfg(target_os = "linux")]
fn sigchld_arrives_when_a_child_exits() {
    giong::block_on(async {
        let mut s = signal::listen(SignalKind::CHILD).unwrap();
        let mut child = giong::process::Command::new("true").spawn().unwrap();
        timeout(Duration::from_secs(5), s.recv()).await.unwrap();
        // Listening did not reap it.
        assert!(child.wait().await.unwrap().success());
    });
}

/// `SIGTERM` listened for here still shuts down a runtime built with
/// `shutdown_on_signals`, whichever was set up first.
#[test]
fn a_sigterm_listener_and_shutdown_on_signals_work_together() {
    let mut early = signal::listen(SignalKind::TERMINATE).unwrap();
    giong::Builder::new()
        .workers(1)
        .shutdown_on_signals(true)
        .build()
        .unwrap()
        .run(|_| async {
            let mut late = signal::listen(SignalKind::TERMINATE).unwrap();
            raise(libc::SIGTERM);
            timeout(Duration::from_secs(5), late.recv()).await.unwrap();
            timeout(Duration::from_secs(5), giong::shutdown_signal())
                .await
                .unwrap();
        })
        .unwrap();
    giong::block_on(async move {
        timeout(Duration::from_secs(5), early.recv()).await.unwrap();
    });
}
