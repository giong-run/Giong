//! The budget of immediate operations a task has per poll.
//!
//! An operation that finishes at once does not yield, so a task whose every
//! read and write succeeds immediately could keep the worker to itself.
//! Once a task has spent its budget, operations report "pending" and wake
//! the task again, which puts it at the back of the run queue.

use std::cell::Cell;

/// Immediate operations a task may complete in one poll before it yields.
pub(crate) const BUDGET: u32 = 128;

thread_local! {
    /// What is left of the current task's [`BUDGET`].
    static LEFT: Cell<u32> = const { Cell::new(BUDGET) };
}

/// Gives the task about to be polled a full budget. Called by the executor.
pub(crate) fn refill_budget() {
    LEFT.with(|left| left.set(BUDGET));
}

/// Takes one unit of the current task's budget; `false` if it is spent.
/// Both drivers count immediate sends of datagrams against it.
pub(crate) fn spend_budget() -> bool {
    LEFT.with(|left| match left.get() {
        0 => false,
        n => {
            left.set(n - 1);
            true
        }
    })
}

/// Returns a unit taken by [`spend_budget`] for work that was not done.
pub(crate) fn give_back_budget() {
    LEFT.with(|left| left.set(left.get().saturating_add(1).min(BUDGET)));
}
