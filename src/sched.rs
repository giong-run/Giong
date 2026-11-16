//! What the runtime tells the kernel's CPU scheduler about its threads.
//!
//! Linux's fair scheduler (EEVDF, since 6.6) decides which runnable thread
//! gets a CPU by virtual deadline: a thread's deadline is its virtual run
//! time plus its *slice*. Every thread gets the same default slice (about
//! 3 ms on most machines), so when every CPU is busy a worker that wakes up
//! because a packet arrived may wait for the thread on its CPU to use up a
//! slice before it runs, whatever the two threads' nice values are. Nice
//! changes how much CPU a thread gets over time, not how soon it runs.
//!
//! Two calls fix that without any privilege:
//!
//! * [`set_slice`] gives a worker a short slice (Linux 6.12+ reads
//!   `sched_runtime` as the requested slice for normal threads). A short
//!   slice means an early deadline, so a waking worker is picked first, and
//!   since Linux 6.13 it also preempts the running thread at once. The worker
//!   gets no more CPU over time than before: its share still comes from its
//!   weight.
//! * [`set_idle_class`] puts a compute thread in `SCHED_IDLE`. A normal
//!   thread that wakes preempts a `SCHED_IDLE` thread on its CPU at once,
//!   which nice +10 does not do.
//!
//! Both fail quietly on older kernels or where a seccomp profile refuses
//! `sched_setattr`: the thread keeps the defaults it had.

use std::time::Duration;

/// `struct sched_attr` from `include/uapi/linux/sched/types.h` (version 1,
/// 56 bytes; the kernel accepts any known size).
#[repr(C)]
#[derive(Default)]
struct SchedAttr {
    size: u32,
    sched_policy: u32,
    sched_flags: u64,
    sched_nice: i32,
    sched_priority: u32,
    sched_runtime: u64,
    sched_deadline: u64,
    sched_period: u64,
    sched_util_min: u32,
    sched_util_max: u32,
}

const SCHED_OTHER: u32 = 0;
const SCHED_IDLE: u32 = 5;

/// The kernel clamps a requested slice to 0.1 ms ..= 100 ms.
const MIN_SLICE: Duration = Duration::from_micros(100);
const MAX_SLICE: Duration = Duration::from_millis(100);

fn set_attr(attr: &mut SchedAttr) -> bool {
    attr.size = std::mem::size_of::<SchedAttr>() as u32;
    // SAFETY: `attr` is a valid, initialised `sched_attr` of the size we pass;
    // pid 0 is the calling thread. The kernel only reads it.
    unsafe { libc::syscall(libc::SYS_sched_setattr, 0, attr as *mut SchedAttr, 0) == 0 }
}

/// The calling thread's nice value, kept across `sched_setattr`, which
/// would otherwise reset it to 0.
fn current_nice() -> i32 {
    // SAFETY: plain syscall. -1 is also a valid nice value; errno is not
    // worth checking here because who == 0 cannot fail.
    unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
}

/// Asks for a scheduler slice of `slice` for the calling thread (clamped to
/// what the kernel accepts). Returns `false` where the kernel refused.
pub(crate) fn set_slice(slice: Duration) -> bool {
    let slice = slice.clamp(MIN_SLICE, MAX_SLICE);
    let mut attr = SchedAttr {
        sched_policy: SCHED_OTHER,
        sched_nice: current_nice(),
        sched_runtime: slice.as_nanos() as u64,
        ..SchedAttr::default()
    };
    set_attr(&mut attr) && slice_of_current() == Some(slice.as_nanos() as u64)
}

/// Puts the calling thread in `SCHED_IDLE`. Returns `false` where the kernel
/// refused.
pub(crate) fn set_idle_class() -> bool {
    let mut attr = SchedAttr {
        sched_policy: SCHED_IDLE,
        ..SchedAttr::default()
    };
    set_attr(&mut attr)
}

/// The slice the calling thread runs with, if the kernel reports one
/// (kernels before 6.12 report 0 for normal threads).
pub(crate) fn slice_of_current() -> Option<u64> {
    let mut attr = SchedAttr::default();
    let size = std::mem::size_of::<SchedAttr>() as u32;
    // SAFETY: the kernel writes at most `size` bytes into `attr`.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_sched_getattr,
            0,
            &mut attr as *mut SchedAttr,
            size,
            0,
        )
    };
    (rc == 0 && attr.sched_runtime != 0).then_some(attr.sched_runtime)
}

/// The calling thread's scheduling policy.
#[cfg(test)]
pub(crate) fn policy_of_current() -> u32 {
    let mut attr = SchedAttr::default();
    let size = std::mem::size_of::<SchedAttr>() as u32;
    // SAFETY: as above.
    unsafe {
        libc::syscall(
            libc::SYS_sched_getattr,
            0,
            &mut attr as *mut SchedAttr,
            size,
            0,
        )
    };
    attr.sched_policy
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slice_is_set_on_the_calling_thread_only_and_keeps_nice() {
        std::thread::spawn(|| {
            // SAFETY: plain syscall on the calling thread.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 3) };
            if set_slice(Duration::from_micros(250)) {
                assert_eq!(slice_of_current(), Some(250_000));
                assert_eq!(current_nice(), 3);
            }
        })
        .join()
        .unwrap();
        assert_ne!(slice_of_current(), Some(250_000));
    }

    #[test]
    fn idle_class_is_per_thread() {
        std::thread::spawn(|| {
            if set_idle_class() {
                assert_eq!(policy_of_current(), SCHED_IDLE);
            }
        })
        .join()
        .unwrap();
        assert_eq!(policy_of_current(), SCHED_OTHER);
    }
}
