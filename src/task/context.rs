//! The context a piece of work runs in: its deadline and its task-local
//! values. It lives in a thread-local while a wrapping future
//! ([`LocalScope`](super::LocalScope), a scope's run, a task group's
//! task) polls what it wraps, and is restored
//! when that poll returns, by unwinding too.
//!
//! The budget a piece of work runs in is not here: it lives in the worker
//! (`crate::budget`), and scope children and task-group tasks get it the
//! way any code on the task does (children run on the task; group tasks
//! are spawned, and spawned tasks inherit the budget).

use std::any::Any;
use std::cell::Cell;
use std::ptr;
use std::rc::Rc;
use std::time::Instant;

/// A deadline as the event loop's clock and, on Linux, as the kernel's:
/// `CLOCK_MONOTONIC`, which is also what `Instant` reads there.
#[derive(Clone, Copy, Debug)]
pub(crate) struct DeadlineAt {
    pub(crate) at: Instant,
    /// The same time as an absolute `CLOCK_MONOTONIC` timespec, never
    /// earlier than `at`, for a linked timeout (see `kernel_deadline`).
    #[cfg(target_os = "linux")]
    sec: u64,
    #[cfg(target_os = "linux")]
    nsec: u32,
}

impl DeadlineAt {
    #[cfg(target_os = "linux")]
    pub(crate) fn new(at: Instant) -> Self {
        let (base, base_ns) = clock_base();
        // Instants before the base cannot be deadlines still to come.
        let ns = base_ns.saturating_add(at.saturating_duration_since(base).as_nanos() as u64);
        Self {
            at,
            sec: ns / 1_000_000_000,
            nsec: (ns % 1_000_000_000) as u32,
        }
    }

    #[cfg(not(target_os = "linux"))]
    pub(crate) fn new(at: Instant) -> Self {
        Self { at }
    }

    /// The earlier of two deadlines.
    pub(crate) fn min(self, other: Option<DeadlineAt>) -> DeadlineAt {
        match other {
            Some(o) if o.at < self.at => o,
            _ => self,
        }
    }
}

/// An `Instant` and the `CLOCK_MONOTONIC` time in nanoseconds that is
/// never earlier than it, taken once per thread: `Instant` reads the same
/// clock, so a deadline converts by adding the same offset, without reading
/// the clock again.
#[cfg(target_os = "linux")]
fn clock_base() -> (Instant, u64) {
    thread_local! {
        static BASE: (Instant, u64) = {
            // `Instant` first: the timespec read after it is at least as
            // late, so the kernel never fires before a deadline has passed.
            let instant = Instant::now();
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: `clock_gettime` writes the timespec it is given.
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
            (instant, ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64)
        };
    }
    BASE.with(|b| *b)
}

/// One task-local value, or the values a snapshot carries.
pub(crate) enum Frame<'a> {
    One { key: usize, value: &'a Rc<dyn Any> },
    Many(&'a [(usize, Rc<dyn Any>)]),
}

/// A link of the chain of task-local frames, innermost first. It lives on
/// the stack of the poll that set it.
pub(crate) struct LocalFrame<'a> {
    parent: *const LocalFrame<'static>,
    frame: Frame<'a>,
}

struct Cx {
    deadline: Cell<Option<DeadlineAt>>,
    locals: Cell<*const LocalFrame<'static>>,
}

thread_local! {
    static CX: Cx = const {
        Cx {
            deadline: Cell::new(None),
            locals: Cell::new(ptr::null()),
        }
    };
}

/// The deadline in force, for an io_uring operation to link (see
/// `Driver::start`): an absolute `CLOCK_MONOTONIC` time as
/// `(seconds, nanoseconds)`.
#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn kernel_deadline() -> Option<(u64, u32)> {
    CX.try_with(|cx| cx.deadline.get().map(|d| (d.sec, d.nsec)))
        .ok()
        .flatten()
}

/// Whether an operation cut by a linked deadline may be submitted again
/// where it is polled now: not under a deadline that has passed too, which
/// would cut it again at once (a scope never polls its body then, so this
/// is only a guard against a loop).
#[cfg(target_os = "linux")]
#[cold]
#[inline(never)]
pub(crate) fn restart_after_cut() -> bool {
    match current_deadline() {
        Some(d) => d.at > std::time::Instant::now(),
        None => true,
    }
}

/// The deadline in force.
pub(crate) fn current_deadline() -> Option<DeadlineAt> {
    CX.try_with(|cx| cx.deadline.get()).ok().flatten()
}

/// Restores the context when the poll that changed it returns or unwinds.
pub(crate) struct Restore {
    deadline: Option<DeadlineAt>,
    locals: *const LocalFrame<'static>,
}

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = CX.try_with(|cx| {
            cx.deadline.set(self.deadline);
            cx.locals.set(self.locals);
        });
    }
}

fn save() -> Restore {
    CX.with(|cx| Restore {
        deadline: cx.deadline.get(),
        locals: cx.locals.get(),
    })
}

/// Sets the deadline until the returned guard drops. The earlier of `at`
/// and the deadline already in force wins.
pub(crate) fn enter_deadline(at: DeadlineAt) -> Restore {
    let restore = save();
    CX.with(|cx| cx.deadline.set(Some(at.min(restore.deadline))));
    restore
}

/// Runs `f` with `frame` pushed onto the task-local chain.
pub(crate) fn with_frame<R>(frame: Frame<'_>, f: impl FnOnce() -> R) -> R {
    let restore = save();
    let link = LocalFrame {
        parent: restore.locals,
        frame,
    };
    // The frame lives on this stack until `restore` puts the old pointer
    // back, which happens before `link` goes out of scope (it is dropped
    // first: locals drop in reverse order).
    let link_ptr = (&link as *const LocalFrame<'_>).cast::<LocalFrame<'static>>();
    CX.with(|cx| cx.locals.set(link_ptr));
    let out = f();
    drop(restore);
    out
}

/// Finds the innermost value for `key` and lends it to `f`.
pub(crate) fn find_local<R>(key: usize, f: impl FnOnce(Option<&Rc<dyn Any>>) -> R) -> R {
    let mut node = CX.try_with(|cx| cx.locals.get()).unwrap_or(ptr::null());
    while !node.is_null() {
        // SAFETY: every pointer in the chain points at a `LocalFrame` on the
        // stack of a poll that is still running further up this thread's
        // stack (see `with_frame`), so it is alive and not mutated.
        let link = unsafe { &*node };
        match &link.frame {
            Frame::One { key: k, value } if *k == key => return f(Some(value)),
            Frame::Many(values) => {
                if let Some((_, value)) = values.iter().find(|(k, _)| *k == key) {
                    return f(Some(value));
                }
            }
            Frame::One { .. } => {}
        }
        node = link.parent;
    }
    f(None)
}

/// The context in force, owned, for a task that runs later on its own: a
/// [`TaskGroup`](super::TaskGroup)'s.
#[derive(Default)]
pub(crate) struct Snapshot {
    pub(crate) deadline: Option<DeadlineAt>,
    locals: Vec<(usize, Rc<dyn Any>)>,
}

impl Snapshot {
    pub(crate) fn capture() -> Snapshot {
        let mut locals: Vec<(usize, Rc<dyn Any>)> = Vec::new();
        let mut node = CX.try_with(|cx| cx.locals.get()).unwrap_or(ptr::null());
        let mut push = |key: usize, value: &Rc<dyn Any>| {
            // Innermost first; an outer value for the same key is shadowed.
            if !locals.iter().any(|(k, _)| *k == key) {
                locals.push((key, value.clone()));
            }
        };
        while !node.is_null() {
            // SAFETY: as in `find_local`.
            let link = unsafe { &*node };
            match &link.frame {
                Frame::One { key, value } => push(*key, value),
                Frame::Many(values) => values.iter().for_each(|(k, v)| push(*k, v)),
            }
            node = link.parent;
        }
        Snapshot {
            deadline: current_deadline(),
            locals,
        }
    }

    /// Runs `f` inside this context.
    pub(crate) fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _deadline = self.deadline.map(enter_deadline);
        if self.locals.is_empty() {
            f()
        } else {
            with_frame(Frame::Many(&self.locals), f)
        }
    }
}
