//! The budget of immediate operations a task has per poll.
//!
//! An operation that finishes at once does not yield, so a task whose every
//! read and write succeeds immediately could keep the worker to itself.
//! Once a task has spent its budget, operations report "pending" and wake
//! the task again, which puts it at the back of the run queue.
//!
//! Only a worker thread has a budget: the executor refills it before every
//! poll there. Off a worker (a future polled by another executor, or after
//! `giong::block_on` returned), nothing would refill it, so once it runs out
//! there it is lifted for good.

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
/// Both drivers count immediate sends of datagrams against it, and the
/// channels count values that were ready at once.
pub(crate) fn spend_budget() -> bool {
    LEFT.with(|left| match left.get() {
        0 => spent(left),
        n => {
            left.set(n - 1);
            true
        }
    })
}

/// [`spend_budget`] with nothing left: `false` on a worker, where the next
/// poll refills it. Elsewhere nothing ever would, so the budget is lifted.
#[cold]
fn spent(left: &Cell<u32>) -> bool {
    if crate::runtime::current_uid() != 0 {
        return false;
    }
    left.set(u32::MAX);
    true
}

/// Returns a unit taken by [`spend_budget`] for work that was not done.
pub(crate) fn give_back_budget() {
    LEFT.with(|left| left.set(left.get().saturating_add(1).min(BUDGET)));
}

/// For a channel operation that can finish at once: takes one unit of the
/// budget, or, with none left, wakes the task again and returns `false`, and
/// the caller reports "pending" instead.
pub(crate) fn spend_or_yield(cx: &std::task::Context<'_>) -> bool {
    if spend_budget() {
        return true;
    }
    cx.waker().wake_by_ref();
    false
}
