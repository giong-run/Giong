//! The budget of immediate operations a task has per poll.
//!
//! An operation that finishes at once does not yield, so a task whose every
//! read and write succeeds immediately could keep the worker to itself.
//! Once a task has spent its budget, operations report "pending" and wake
//! the task again, which puts it at the back of the run queue.
//!
//! Only a worker thread has a budget: it is refilled before every poll
//! there. Off a worker (a future polled by another executor, or a
//! `giong::block_on` that has returned), nothing would refill it, so
//! spending never runs out.

use std::cell::Cell;

/// Immediate operations a task may complete in one poll before it yields.
pub(crate) const BUDGET: u32 = 128;

/// What [`LEFT`] holds on a thread that is not running a worker.
const UNLIMITED: u32 = u32::MAX;

thread_local! {
    /// What is left of the current task's [`BUDGET`], or [`UNLIMITED`].
    static LEFT: Cell<u32> = const { Cell::new(UNLIMITED) };
}

/// Starts (`true`) or stops counting the budget on this thread, as a worker
/// starts or stops running on it.
pub(crate) fn count_budget(on: bool) {
    let _ = LEFT.try_with(|left| left.set(if on { BUDGET } else { UNLIMITED }));
}

/// Gives the task about to be polled a full budget. Called by the executor.
pub(crate) fn refill_budget() {
    LEFT.with(|left| left.set(BUDGET));
}

/// Takes one unit of the current task's budget; `false` if it is spent.
/// Both drivers count immediate sends of datagrams against it, and the
/// channels count values that were ready at once.
pub(crate) fn spend_budget() -> bool {
    LEFT.with(|left| match left.get() {
        0 => false,
        UNLIMITED => true,
        n => {
            left.set(n - 1);
            true
        }
    })
}

/// Returns a unit taken by [`spend_budget`] for work that was not done.
pub(crate) fn give_back_budget() {
    LEFT.with(|left| {
        if left.get() < BUDGET {
            left.set(left.get() + 1);
        }
    });
}
