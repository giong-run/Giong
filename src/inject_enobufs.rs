//! Failures handed out in place of real `ENOBUFS` errors (tests only).
//!
//! The counters are process-wide, so a test that sets them must not run
//! alongside another one that sends or connects.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

static SENDS: AtomicU32 = AtomicU32::new(0);
static CONNECTS: AtomicU32 = AtomicU32::new(0);
static LATE_CONNECTS: AtomicU32 = AtomicU32::new(0);
static SOCKETS: AtomicU32 = AtomicU32::new(0);
static HANDED_OUT: AtomicU32 = AtomicU32::new(0);

/// The next `n` stream sends (`send`, `sendmsg`) fail with `ENOBUFS`
/// without reaching the kernel.
pub fn fail_sends(n: u32) {
    SENDS.store(n, Relaxed);
}

/// The next `n` calls to `connect(2)` fail with `ENOBUFS` without reaching
/// the kernel, as when the kernel had no buffer for the SYN.
pub fn fail_connects(n: u32) {
    CONNECTS.store(n, Relaxed);
}

/// The next `n` connects that succeed are reported as failed with
/// `ENOBUFS` once the handshake is over, as when `SO_ERROR` says the
/// shortage broke it. The socket is left as it is.
pub fn fail_connects_late(n: u32) {
    LATE_CONNECTS.store(n, Relaxed);
}

/// The next `n` sockets a stream connect makes fail with `ENOBUFS`
/// without reaching the kernel, as when `socket(2)` had no buffer for them.
pub fn fail_sockets(n: u32) {
    SOCKETS.store(n, Relaxed);
}

/// How many failures were handed out since the last [`reset`].
pub fn handed_out() -> u32 {
    HANDED_OUT.load(Relaxed)
}

/// No more failures, and the count back to zero.
pub fn reset() {
    SENDS.store(0, Relaxed);
    CONNECTS.store(0, Relaxed);
    LATE_CONNECTS.store(0, Relaxed);
    SOCKETS.store(0, Relaxed);
    HANDED_OUT.store(0, Relaxed);
}

fn take(counter: &AtomicU32) -> bool {
    let taken = counter
        .fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1))
        .is_ok();
    if taken {
        HANDED_OUT.fetch_add(1, Relaxed);
    }
    taken
}

pub(crate) fn send() -> bool {
    take(&SENDS)
}

pub(crate) fn connect() -> bool {
    take(&CONNECTS)
}

pub(crate) fn late_connect() -> bool {
    take(&LATE_CONNECTS)
}

pub(crate) fn socket() -> bool {
    take(&SOCKETS)
}
