//! A look inside a running runtime, for diagnostics.
//!
//! Every worker keeps a few counters and says which task it is polling;
//! any thread reads them with [`Handle::pulse`](crate::Handle::pulse),
//! without disturbing the worker. That is enough to see that a worker is
//! stuck, and in which task: one whose `polls` count stops moving while it
//! is not asleep is stuck in the task named by `polling`.
//!
//! [`snapshot`] goes further (live tasks grouped by where they were spawned,
//! timers, I/O in flight, and the [`Source`]s registered on the worker, such
//! as connection pools), but runs on the worker it describes: send it there
//! with [`Handle::spawn_on`](crate::Handle::spawn_on).
//!
//! [`Handle::set_profiling`](crate::Handle::set_profiling) makes every
//! worker time each task poll, grouped by where the task was spawned;
//! [`take_profile`] collects the result on a worker. Turned off, profiling
//! costs one relaxed load per batch of tasks.
//!
//! Tasks are named by the place in the source where they were spawned
//! ([`spawn`](crate::spawn), [`Handle::spawn_on`](crate::Handle::spawn_on),
//! [`Handle::spawn_balanced`](crate::Handle::spawn_balanced)), and a
//! worker's main future by the call that started the runtime. A task spawned
//! inside a library is named by the library's line, for example the
//! connection tasks of `giong-http`'s server.
//!
//! The `giong-mcp` crate serves all of this to AI agents over the Model
//! Context Protocol.
//!
//! ```no_run
//! # fn main() -> std::io::Result<()> {
//! let rt = giong::Builder::new().workers(2).build()?;
//! let handle = rt.handle();
//! std::thread::spawn(move || loop {
//!     std::thread::sleep(std::time::Duration::from_secs(1));
//!     for w in 0..handle.workers() {
//!         let pulse = handle.pulse(w);
//!         println!("worker {w}: {} polls, polling {:?}", pulse.polls, pulse.polling);
//!     }
//! });
//! rt.run(|_| async { giong::shutdown_signal().await })
//! # }
//! ```

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::panic;
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, Ordering};
use std::time::Duration;

use crate::runtime;
use crate::DriverKind;

/// Where a task was spawned: a file, a line and a column.
pub type Location = &'static panic::Location<'static>;

/// The counters a worker publishes for other threads (see [`Pulse`]).
///
/// Written only by the worker, with relaxed stores, on a cache line of its
/// own; readers may see them a little late.
#[repr(align(64))]
pub(crate) struct Counters {
    ticks: AtomicU64,
    polls: AtomicU64,
    parks: AtomicU64,
    idle_ns: AtomicU64,
    spawned: AtomicU64,
    /// The location of the task being polled, or null between batches.
    polling: AtomicPtr<panic::Location<'static>>,
    /// 0 until the worker has a driver, then 1 for io_uring, 2 for epoll.
    driver: AtomicU8,
}

impl Counters {
    pub(crate) fn new() -> Self {
        Self {
            ticks: AtomicU64::new(0),
            polls: AtomicU64::new(0),
            parks: AtomicU64::new(0),
            idle_ns: AtomicU64::new(0),
            spawned: AtomicU64::new(0),
            polling: AtomicPtr::new(ptr::null_mut()),
            driver: AtomicU8::new(0),
        }
    }

    pub(crate) fn set_driver(&self, kind: DriverKind) {
        let code = match kind {
            DriverKind::IoUring => 1,
            DriverKind::Epoll => 2,
        };
        self.driver.store(code, Ordering::Relaxed);
    }

    pub(crate) fn driver(&self) -> Option<DriverKind> {
        match self.driver.load(Ordering::Relaxed) {
            1 => Some(DriverKind::IoUring),
            2 => Some(DriverKind::Epoll),
            _ => None,
        }
    }

    // The worker is the only writer, so a load and a store are enough (no
    // read-modify-write instruction).
    fn bump(counter: &AtomicU64, by: u64) {
        counter.store(
            counter.load(Ordering::Relaxed).wrapping_add(by),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn tick(&self) {
        Self::bump(&self.ticks, 1);
    }

    pub(crate) fn spawned(&self) {
        Self::bump(&self.spawned, 1);
    }

    pub(crate) fn parked(&self, slept: Duration) {
        Self::bump(&self.parks, 1);
        Self::bump(
            &self.idle_ns,
            u64::try_from(slept.as_nanos()).unwrap_or(u64::MAX),
        );
    }

    /// A task spawned at `location` is about to be polled.
    pub(crate) fn polling(&self, location: Location) {
        self.polling.store(
            location as *const panic::Location<'static> as *mut _,
            Ordering::Relaxed,
        );
        Self::bump(&self.polls, 1);
    }

    /// The batch of tasks is over.
    pub(crate) fn between_polls(&self) {
        self.polling.store(ptr::null_mut(), Ordering::Relaxed);
    }

    pub(crate) fn read(&self) -> (u64, u64, u64, Duration, u64, Option<Location>) {
        let polling = self.polling.load(Ordering::Relaxed);
        (
            self.ticks.load(Ordering::Relaxed),
            self.polls.load(Ordering::Relaxed),
            self.parks.load(Ordering::Relaxed),
            Duration::from_nanos(self.idle_ns.load(Ordering::Relaxed)),
            self.spawned.load(Ordering::Relaxed),
            // SAFETY: the pointer is null or came from a `&'static Location`.
            unsafe { polling.as_ref() },
        )
    }
}

/// What a worker is doing, as far as another thread can tell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkerState {
    /// Running tasks or exchanging work with the kernel.
    Running,
    /// Asleep in the kernel, waiting for I/O, a timer or a wake-up.
    Parked,
    /// Its main future has returned and the worker is gone, or it never
    /// started.
    Stopped,
}

/// A worker's counters, read from any thread with
/// [`Handle::pulse`](crate::Handle::pulse) without disturbing the worker.
///
/// The counts start at zero when the runtime is built and only go up; two
/// pulses taken a while apart say how much happened in between. They are
/// published with relaxed stores, so a reader may see them slightly late.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Pulse {
    /// The worker's index.
    pub worker: usize,
    /// What it is doing.
    pub state: WorkerState,
    /// Which I/O driver it runs on; `None` until it has started.
    pub driver: Option<DriverKind>,
    /// Turns of the event loop.
    pub ticks: u64,
    /// Task polls, the main future's included.
    pub polls: u64,
    /// Times the worker went to sleep in the kernel.
    pub parks: u64,
    /// Total time spent asleep in the kernel.
    pub idle: Duration,
    /// Tasks spawned on the worker.
    pub spawned: u64,
    /// Live tasks, as of the last batch.
    pub tasks: usize,
    /// Tasks waiting in the run queue, as of the last batch.
    pub runnable: usize,
    /// Where the task being polled right now was spawned; `None` between
    /// batches of tasks. When `polls` stays the same over a while and
    /// `state` is [`Running`](WorkerState::Running), the worker is stuck in
    /// this task: it blocks the thread or loops without awaiting.
    pub polling: Option<Location>,
}

/// One worker in detail: see [`snapshot`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Snapshot {
    /// The worker's index.
    pub worker: usize,
    /// Which I/O driver it runs on.
    pub driver: DriverKind,
    /// Live tasks, not counting the main future. The task that called
    /// [`snapshot`] is one of them.
    pub tasks: usize,
    /// Tasks waiting in the run queue.
    pub runnable: usize,
    /// Armed timers (sleeps, timeouts).
    pub timers: usize,
    /// I/O operations the driver tracks: in flight, or cancelled and not yet
    /// confirmed by the kernel.
    pub io_in_flight: usize,
    /// Receive buffers in the worker's pool.
    pub recv_buffers: usize,
    /// Receive buffers not holding data.
    pub recv_buffers_free: usize,
    /// The main future has returned and the remaining tasks are finishing
    /// within the shutdown grace period.
    pub draining: bool,
    /// Live tasks grouped by where they were spawned, most first.
    pub tasks_by_location: Vec<(Location, usize)>,
    /// What the worker's registered [`Source`]s report.
    pub sources: Vec<SourceStats>,
}

/// Something on a worker worth looking at from outside, such as a
/// connection pool: see [`register`].
pub trait Source {
    /// What kind of thing it is, such as `"postgres pool"`.
    fn kind(&self) -> &'static str;
    /// Which one it is, such as the server's address.
    fn name(&self) -> String;
    /// Numbers that describe its state now, such as `("idle", 3)`.
    fn stats(&self) -> Vec<(&'static str, u64)>;
}

/// What a [`Source`] reported in a [`Snapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SourceStats {
    /// [`Source::kind`].
    pub kind: &'static str,
    /// [`Source::name`].
    pub name: String,
    /// [`Source::stats`].
    pub stats: Vec<(&'static str, u64)>,
}

/// Time spent polling the tasks spawned at one place: see [`take_profile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PollProfile {
    /// Where the tasks were spawned.
    pub location: Location,
    /// How many polls were timed.
    pub polls: u64,
    /// Their total duration.
    pub total: Duration,
    /// The longest one.
    pub max: Duration,
}

/// The worker-local part: registered sources and the profile being
/// gathered.
#[derive(Default)]
pub(crate) struct Local {
    sources: RefCell<Vec<Weak<dyn Source>>>,
    profile: RefCell<HashMap<usize, PollProfile>>,
}

impl Local {
    /// Adds one timed poll of a task spawned at `location`.
    pub(crate) fn record(&self, location: Location, took: Duration) {
        let key = location as *const panic::Location<'static> as usize;
        let mut profile = self.profile.borrow_mut();
        let entry = profile.entry(key).or_insert(PollProfile {
            location,
            polls: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
        });
        entry.polls += 1;
        entry.total += took;
        entry.max = entry.max.max(took);
    }
}

impl fmt::Debug for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Local")
            .field("sources", &self.sources.borrow().len())
            .finish()
    }
}

/// Describes the current worker in detail.
///
/// To look at another worker, run it there:
/// `handle.spawn_on(w, || async { giong::observe::snapshot() })`.
/// Counting the tasks walks the worker's task table, so it takes time in
/// proportion to the number of tasks (well under a millisecond for ten
/// thousand).
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn snapshot() -> Snapshot {
    runtime::with_worker(|w| {
        let mut by_location: HashMap<usize, (Location, usize)> = HashMap::new();
        let mut tasks = 0;
        for location in w.task_locations() {
            tasks += 1;
            let key = location as *const panic::Location<'static> as usize;
            by_location.entry(key).or_insert((location, 0)).1 += 1;
        }
        let mut tasks_by_location: Vec<_> = by_location.into_values().collect();
        tasks_by_location.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.file().cmp(b.0.file()))
                .then_with(|| a.0.line().cmp(&b.0.line()))
        });
        let sources = {
            let mut list = w.observe.sources.borrow_mut();
            list.retain(|s| s.strong_count() > 0);
            list.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
        };
        // Called outside the borrow: a source may register another one.
        let sources = sources
            .iter()
            .map(|s| SourceStats {
                kind: s.kind(),
                name: s.name(),
                stats: s.stats(),
            })
            .collect();
        let (io_in_flight, timers) = w.io_and_timers();
        Snapshot {
            worker: w.index,
            driver: w.driver_kind(),
            tasks,
            runnable: w.queue.borrow().len(),
            timers,
            io_in_flight,
            recv_buffers: w.pool.count() as usize,
            recv_buffers_free: w.pool.free(),
            draining: w.is_draining(),
            tasks_by_location,
            sources,
        }
    })
}

/// Registers a [`Source`] on the current worker, so that [`snapshot`]
/// reports it. Only a weak reference is kept: the source disappears from
/// snapshots once it is dropped.
///
/// Returns `false`, and does nothing, outside of a giong runtime.
///
/// ```
/// use std::rc::Rc;
/// use giong::observe::{self, Source};
///
/// struct Cache;
/// impl Source for Cache {
///     fn kind(&self) -> &'static str { "cache" }
///     fn name(&self) -> String { "users".into() }
///     fn stats(&self) -> Vec<(&'static str, u64)> { vec![("entries", 42)] }
/// }
///
/// giong::block_on(async {
///     let cache = Rc::new(Cache);
///     let weak: std::rc::Weak<dyn Source> = Rc::downgrade(&cache) as _;
///     assert!(observe::register(weak));
///     let snap = observe::snapshot();
///     assert_eq!(snap.sources[0].stats, vec![("entries", 42)]);
/// });
/// ```
pub fn register(source: Weak<dyn Source>) -> bool {
    runtime::try_with_worker(|w| w.observe.sources.borrow_mut().push(source)).is_some()
}

/// Registers `source` (see [`register`]) and keeps nothing else: a shortcut
/// for a source held in an `Rc`.
pub fn register_rc<S: Source + 'static>(source: &Rc<S>) -> bool {
    let weak: Weak<dyn Source> = Rc::downgrade(source) as Weak<dyn Source>;
    register(weak)
}

/// Returns the poll times gathered on the current worker since profiling
/// was turned on with [`Handle::set_profiling`](crate::Handle::set_profiling)
/// (or since the last call), longest total first, and starts over.
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn take_profile() -> Vec<PollProfile> {
    runtime::with_worker(|w| {
        let mut list: Vec<_> = std::mem::take(&mut *w.observe.profile.borrow_mut())
            .into_values()
            .collect();
        list.sort_by_key(|p| std::cmp::Reverse(p.total));
        list
    })
}
