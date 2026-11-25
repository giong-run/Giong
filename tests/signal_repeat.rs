#![cfg(unix)]
//! A second `SIGTERM` ends the process only while a runtime built with
//! `shutdown_on_signals` is shutting down, never because something listens
//! for `SIGTERM` with `giong::signal`. In a test binary of its own: a second
//! signal during another test's runtime would end this one.

use std::time::Duration;

use giong::signal::{self, SignalKind};
use giong::time::{sleep, timeout};

/// Sends `signal` to a thread of its own (see `tests/signal.rs`).
fn raise(signal: i32) {
    std::thread::spawn(move || {
        // SAFETY: sends a signal to this thread; the process has a handler.
        assert_eq!(unsafe { libc::raise(signal) }, 0);
    })
    .join()
    .unwrap();
}

#[test]
fn a_listener_alone_survives_repeated_sigterm() {
    // A runtime with shutdown_on_signals has come and gone first.
    giong::Builder::new()
        .workers(1)
        .shutdown_on_signals(true)
        .build()
        .unwrap()
        .run(|_| async {})
        .unwrap();
    giong::block_on(async {
        let mut s = signal::listen(SignalKind::TERMINATE).unwrap();
        for _ in 0..3 {
            raise(libc::SIGTERM);
            timeout(Duration::from_secs(5), s.recv()).await.unwrap();
            // Further apart than one request's signals.
            sleep(Duration::from_millis(300)).await;
        }
    });
}
