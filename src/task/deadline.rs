use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::context;
use crate::time::Sleep;

/// The deadline in force where it is called, if any: the earliest of the
/// deadlines of the [scopes](super::Scope) around it, including the ones
/// inherited by task-group tasks.
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

/// `now + duration`, or a time 30 years away when that does not fit in an
/// `Instant`.
pub(crate) fn after(duration: Duration) -> Instant {
    let now = Instant::now();
    now.checked_add(duration)
        .unwrap_or_else(|| now + Duration::from_secs(86400 * 365 * 30))
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
