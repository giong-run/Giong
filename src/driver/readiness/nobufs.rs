//! Waiting out a shortage of network buffers (macOS).
//!
//! macOS takes the buffers of every socket from one pool for the whole
//! machine. When it is empty, `send` and `connect` fail at once with
//! `ENOBUFS`, where Linux makes the sender wait. The socket itself usually
//! has room, so it stays writable and a wait for readiness would spin, and
//! nothing reports that the pool has recovered. So a stream send or connect
//! that gets `ENOBUFS` is tried again on a timer: after 1 ms, then 2, 4, 8,
//! 16 and every 32 ms, each step varied by up to a quarter so that many
//! waiting tasks spread out, for at most [`GIVE_UP`]. The operation stays
//! pending meanwhile, like one waiting for readiness, so the worker runs its
//! other tasks (readers among them, whose reads free buffers), and dropping
//! the future cancels it as usual. `ENOBUFS` is all or nothing on a call, so
//! nothing was sent by the attempts that failed.
//!
//! What this keeps lives only while an operation waits here: in a map apart
//! from the operations, empty otherwise.

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::task::Waker;
use std::time::{Duration, Instant};

use crate::driver::Io;

/// How long an operation is tried again before its `ENOBUFS` is returned.
pub(crate) const GIVE_UP: Duration = Duration::from_secs(2);
/// The first wait.
pub(crate) const FIRST: Duration = Duration::from_millis(1);
/// The longest wait: steps double up to it.
pub(crate) const MOST: Duration = Duration::from_millis(32);

/// `true` if `res`, the result of `io`, is an `ENOBUFS` to wait out: a send
/// on a stream socket, or a connect refused before it began (`connecting`
/// false). A connect that the shortage broke after it began has a dead
/// socket, so the net layer starts over with a new one; a datagram may be
/// dropped, so its send fails at once.
pub(super) fn retries(io: &Io, connecting: bool, res: i32) -> bool {
    if res != -libc::ENOBUFS {
        return false;
    }
    match *io {
        Io::Send { fd, .. } | Io::SendMsg { fd, .. } => is_stream(fd),
        Io::Connect { .. } => !connecting,
        _ => false,
    }
}

fn is_stream(fd: RawFd) -> bool {
    let mut ty: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `ty` and `len` are valid for the call.
    let res = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut ty as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    res == 0 && ty == libc::SOCK_STREAM
}

/// What `connect(2)` returned, on a socket whose last connect may have been
/// refused with `ENOBUFS`: a connect already under way counts as one that
/// has begun, and a socket that is already connected as a success.
pub(super) fn connect_again(res: i32) -> i32 {
    match res {
        r if r == -libc::EALREADY => -libc::EINPROGRESS,
        r if r == -libc::EISCONN => 0,
        r => r,
    }
}

/// One operation waiting for buffers.
struct Wait {
    /// When it first got `ENOBUFS`.
    since: Instant,
    /// The step before the jitter: doubles up to [`MOST`].
    step: Duration,
    /// When it is tried again.
    due: Instant,
    /// Its task, until woken at `due`.
    waker: Option<Waker>,
}

/// The operations waiting for buffers, by key.
pub(super) struct Retries {
    ops: HashMap<usize, Wait>,
    /// For the jitter (xorshift).
    seed: u32,
}

impl Default for Retries {
    fn default() -> Self {
        Self {
            ops: HashMap::new(),
            seed: 0x2545_f491,
        }
    }
}

impl Retries {
    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Plans the next try of operation `key`, which just got `ENOBUFS`.
    /// `false` once it has waited [`GIVE_UP`]: the error is then returned,
    /// and the operation is forgotten here.
    pub(super) fn schedule(&mut self, key: usize, waker: &Waker) -> bool {
        let now = Instant::now();
        let wait = self.ops.entry(key).or_insert(Wait {
            since: now,
            step: Duration::ZERO,
            due: now,
            waker: None,
        });
        let end = wait.since + GIVE_UP;
        if now >= end {
            self.ops.remove(&key);
            return false;
        }
        wait.step = if wait.step.is_zero() {
            FIRST
        } else {
            (wait.step * 2).min(MOST)
        };
        // The last try is made at the end, not after it.
        wait.due = (now + jitter(wait.step, &mut self.seed)).min(end);
        wait.waker = Some(waker.clone());
        true
    }

    /// `true` if operation `key` waits for its next try, which is not due
    /// yet: it is then woken through `waker` when it is.
    pub(super) fn waiting(&mut self, key: usize, waker: &Waker) -> bool {
        let Some(wait) = self.ops.get_mut(&key) else {
            return false;
        };
        if wait.due <= Instant::now() {
            return false;
        }
        match &mut wait.waker {
            Some(w) if w.will_wake(waker) => {}
            w => *w = Some(waker.clone()),
        }
        true
    }

    /// Operation `key` no longer waits here: done, cancelled, or waiting for
    /// readiness instead.
    #[inline]
    pub(super) fn forget(&mut self, key: usize) {
        if !self.ops.is_empty() {
            self.ops.remove(&key);
        }
    }

    /// `timeout` for the poller, cut short to the next try that is due.
    pub(super) fn limit(&self, timeout: Option<Duration>) -> Option<Duration> {
        let now = Instant::now();
        let next = self
            .ops
            .values()
            .filter(|w| w.waker.is_some())
            .map(|w| w.due.saturating_duration_since(now))
            .min();
        match (next, timeout) {
            (Some(next), Some(t)) => Some(next.min(t)),
            (Some(next), None) => Some(next),
            (None, t) => t,
        }
    }

    /// Wakes the tasks whose next try is due.
    pub(super) fn wake_due(&mut self) {
        let now = Instant::now();
        for wait in self.ops.values_mut() {
            if wait.due <= now {
                if let Some(waker) = wait.waker.take() {
                    waker.wake();
                }
            }
        }
    }
}

/// `step`, varied by up to a quarter either way.
fn jitter(step: Duration, seed: &mut u32) -> Duration {
    let mut x = *seed;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *seed = x;
    // From 384/512 to 640/512 of the step.
    step * (384 + x % 257) / 512
}

/// Failures handed out by the tests' hook (`--cfg giong_inject_enobufs`).
#[cfg(giong_inject_enobufs)]
pub(super) mod inject {
    use super::Io;
    use crate::inject_enobufs as hook;

    /// `-ENOBUFS` in place of a send or of a connect not begun yet.
    pub(in crate::driver) fn refused(io: &Io, connecting: bool) -> Option<i32> {
        let fail = match io {
            Io::Send { .. } | Io::SendMsg { .. } => hook::send(),
            Io::Connect { .. } if !connecting => hook::connect(),
            _ => false,
        };
        fail.then_some(-libc::ENOBUFS)
    }

    /// A connect that succeeded with `res == 0`, reported as broken by the
    /// shortage after it began.
    pub(in crate::driver) fn connected(res: i32, connecting: &mut bool) -> i32 {
        if res == 0 && hook::late_connect() {
            *connecting = true;
            return -libc::ENOBUFS;
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::task::Wake;

    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    #[test]
    fn jitter_stays_within_a_quarter() {
        let mut seed = 0x2545_f491;
        for _ in 0..10_000 {
            let d = jitter(Duration::from_millis(32), &mut seed);
            assert!(
                d >= Duration::from_millis(24) && d <= Duration::from_millis(40),
                "{d:?}"
            );
        }
    }

    #[test]
    fn steps_double_up_to_the_most_and_end_at_give_up() {
        let mut r = Retries::default();
        let waker = &Waker::from(Arc::new(Noop));
        assert!(r.schedule(7, waker));
        let mut steps = vec![r.ops[&7].step];
        for _ in 0..7 {
            assert!(r.schedule(7, waker));
            steps.push(r.ops[&7].step);
        }
        let ms: Vec<u64> = steps.iter().map(|d| d.as_millis() as u64).collect();
        assert_eq!(ms, [1, 2, 4, 8, 16, 32, 32, 32]);
        let end = r.ops[&7].since + GIVE_UP;
        assert!(r.ops[&7].due <= end);
        r.ops.get_mut(&7).unwrap().since -= GIVE_UP;
        assert!(!r.schedule(7, waker));
        assert!(r.is_empty());
    }

    #[test]
    fn limit_cuts_the_timeout_to_the_next_try() {
        let mut r = Retries::default();
        assert_eq!(r.limit(None), None);
        assert!(r.schedule(1, &Waker::from(Arc::new(Noop))));
        let t = r.limit(None).unwrap();
        assert!(t <= Duration::from_millis(2));
        assert!(r.limit(Some(Duration::ZERO)).unwrap().is_zero());
        r.forget(1);
        assert_eq!(
            r.limit(Some(Duration::from_secs(1))),
            Some(Duration::from_secs(1))
        );
    }
}
