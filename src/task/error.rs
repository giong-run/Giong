use std::fmt;
use std::io;

use crate::error::{raise, Code};

/// Why work was cancelled: see [`Cancelled::reason`].
///
/// More reasons may be added, so a `match` needs a `_` arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CancelReason {
    /// The deadline of a scope ([`Scope::with_deadline`](super::Scope::with_deadline),
    /// [`Scope::with_timeout`](super::Scope::with_timeout)) passed.
    DeadlineExceeded,
    /// [`Scope::cancel`](super::Scope::cancel) was called.
    Requested,
}

/// The error of work that was cancelled before it finished, and why.
///
/// As an [`io::Error`] (through `?`) it carries a code:
/// [`GIONG-E0024`](crate::error::Code::DeadlineExceeded) with kind
/// `TimedOut` for a deadline,
/// [`GIONG-E0025`](crate::error::Code::ScopeCancelled) for a cancelled
/// scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cancelled {
    reason: CancelReason,
}

impl Cancelled {
    pub(crate) const fn deadline() -> Self {
        Self {
            reason: CancelReason::DeadlineExceeded,
        }
    }

    pub(crate) const fn requested() -> Self {
        Self {
            reason: CancelReason::Requested,
        }
    }

    /// Why the work was cancelled.
    pub fn reason(&self) -> CancelReason {
        self.reason
    }

    /// `true` if a deadline passed.
    pub fn is_deadline_exceeded(&self) -> bool {
        self.reason == CancelReason::DeadlineExceeded
    }
}

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            CancelReason::DeadlineExceeded => f.write_str("the deadline passed"),
            CancelReason::Requested => f.write_str("the scope was cancelled"),
        }
    }
}

impl std::error::Error for Cancelled {}

impl From<Cancelled> for io::Error {
    fn from(c: Cancelled) -> Self {
        match c.reason {
            CancelReason::DeadlineExceeded => raise(
                Code::DeadlineExceeded,
                "the work did not finish before its deadline (giong::task::Scope::with_deadline \
                 or Scope::with_timeout), so it was cancelled",
                "give it more time, or find out what is slow: giong::task::remaining() inside \
                 the work tells how much time is left at each step.",
            ),
            CancelReason::Requested => raise(
                Code::ScopeCancelled,
                "Scope::cancel was called before the scope's work finished, so it was dropped",
                "nothing to fix if the cancel was meant; otherwise find the code that calls \
                 Scope::cancel.",
            ),
        }
    }
}
