use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::context::{self, DeadlineAt};
use super::Cancelled;
use crate::time::{sleep_until, Sleep};

/// Runs `fut` with a deadline that everything inside it inherits: see the
/// [module docs](crate::task#deadlines).
///
/// Resolves to `Ok` with the output of `fut`, or to
/// `Err(`[`Cancelled`]`)` once `at` has passed; `fut` is then dropped, which
/// cancels its I/O. A deadline already in force that is earlier than `at`
/// wins.
pub fn with_deadline<F: Future>(at: Instant, fut: F) -> Deadline<F> {
    Deadline {
        fut,
        at,
        kernel: None,
        timer: sleep_until(at),
        armed: None,
    }
}

/// [`with_deadline`] at `duration` from now.
pub fn with_timeout<F: Future>(duration: Duration, fut: F) -> Deadline<F> {
    let now = Instant::now();
    let at = now
        .checked_add(duration)
        .unwrap_or_else(|| now + Duration::from_secs(86400 * 365 * 30));
    with_deadline(at, fut)
}

/// The deadline in force where it is called, if any: the earliest of the
/// [`with_deadline`] and [`with_timeout`] calls around it, including the
/// ones inherited by scope children and task-group tasks.
///
/// For a library that hands the time left to something else, such as a
/// database statement timeout or a `grpc-timeout` header.
pub fn deadline() -> Option<Instant> {
    context::current_deadline().map(|d| d.at)
}

/// The time left until [`deadline`], or `None` without one. Zero once it
/// has passed.
pub fn remaining() -> Option<Duration> {
    deadline().map(|at| at.saturating_duration_since(Instant::now()))
}

/// Future returned by [`with_deadline`] and [`with_timeout`].
#[must_use = "futures do nothing unless awaited"]
pub struct Deadline<F> {
    /// Pinned with the `Deadline`: never moved out.
    fut: F,
    at: Instant,
    /// `at` as the kernel's clock, worked out at the first poll.
    kernel: Option<DeadlineAt>,
    timer: Sleep,
    /// The waker the armed timer wakes: while it is the same, the timer
    /// needs no new poll.
    armed: Option<Waker>,
}

impl<F> Deadline<F> {
    /// The instant at which the time runs out (an outer, earlier deadline
    /// may end it first).
    pub fn deadline(&self) -> Instant {
        self.at
    }
}

impl<F: Future> Future for Deadline<F> {
    type Output = Result<F::Output, Cancelled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `fut` is structurally pinned: it is only reached through
        // the pinned reference made here and never moved. The other fields
        // are `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        // Checked before `fut` runs: code inside never runs past its
        // deadline, whichever driver noticed first.
        if Instant::now() >= this.at {
            return Poll::Ready(Err(Cancelled::deadline()));
        }
        let at = *this.kernel.get_or_insert_with(|| DeadlineAt::new(this.at));
        let polled = {
            let _restore = context::enter_deadline(at);
            // SAFETY: `fut` is structurally pinned (see above).
            unsafe { Pin::new_unchecked(&mut this.fut) }.poll(cx)
        };
        if let Poll::Ready(value) = polled {
            return Poll::Ready(Ok(value));
        }
        poll_timer(&mut this.timer, &mut this.armed, cx).map(|()| Err(Cancelled::deadline()))
    }
}

impl<F> fmt::Debug for Deadline<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deadline")
            .field("deadline", &self.at)
            .finish_non_exhaustive()
    }
}

/// Polls a deadline's timer, unless it is already armed to wake the same
/// waker: the caller has just checked the clock, and the timer, never
/// early, wakes it once the deadline has passed.
pub(crate) fn poll_timer(
    timer: &mut Sleep,
    armed: &mut Option<Waker>,
    cx: &mut Context<'_>,
) -> Poll<()> {
    if let Some(w) = armed {
        if w.will_wake(cx.waker()) {
            return Poll::Pending;
        }
    }
    let polled = Pin::new(timer).poll(cx);
    *armed = polled.is_pending().then(|| cx.waker().clone());
    polled
}
