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
//! # Why a request is slow
//!
//! [`Handle::set_request_tracing`](crate::Handle::set_request_tracing)
//! makes every worker follow each request from its first bytes to the last
//! byte of its answer, and split its time into phases: receiving it, its
//! handler's own polls, waits for pool connections, database commands and
//! other services (timed with [`wait`]), the rest of the handler's time,
//! and writing the answer. It also notes every task poll of [`LONG_POLL`]
//! or more, so that a request that waited while another task held its
//! worker says which task. Requests are grouped by route ([`set_route`];
//! `giong-web` names them), and [`take_requests`] collects, per route, how
//! many there were, their latency quantiles, where the time of the slowest
//! went ([`RouteStats::breakdown`]), and the slowest few in full.
//!
//! The protocol crates do the following ([`RequestTrace`]); turned off, it
//! costs one read of a thread-local per request, and one per database
//! command. Turned on, each request reads the clock about six times, plus
//! twice per poll of its handler and twice per timed wait, and every task
//! poll is timed (two clock reads), as with profiling.
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
use std::rc::Weak;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicU8, Ordering};
use std::time::{Duration, Instant};

use crate::runtime;
use crate::DriverKind;

mod requests;

pub(crate) use requests::{clear_entered, set_on as set_request_tracing_here, Requests};
pub use requests::{
    set_route, take_requests, wait, BlockedBy, Breakdown, RequestRecord, RequestReport,
    RequestTrace, Route, RouteStats, Wait, WaitStats, WaitTimer, LONG_POLL,
};

pub use crate::runtime::{worker_stats, WorkerStats};

/// Where a task was spawned: a file, a line and a column.
pub type Location = &'static panic::Location<'static>;

/// Set in [`Counters::idle_ns`] while the worker is asleep.
const ASLEEP: u64 = 1 << 63;

/// Set in [`Counters::idle_ns`], with [`ASLEEP`] clear, while the worker
/// starts a sleep: it is reading the clock for it. Totals stay below 2^62
/// (146 years), so this bit is free in them.
const OPENING: u64 = 1 << 62;

/// How long a reader waits for a worker that is starting a sleep to say
/// when it began. Two instructions apart, unless the worker's thread was
/// preempted in between.
const OPENING_WAIT: Duration = Duration::from_millis(20);

/// Nanoseconds on a clock that is the same for every thread of the process,
/// below 2^63 (292 years): what a sleep in progress is measured on, by the
/// worker that sleeps and by the threads that read its pulse.
///
/// On Unix it is `CLOCK_MONOTONIC` read directly: a worker reads it twice
/// per sleep, and going through `Instant` (a subtraction from an epoch,
/// then a `Duration` turned back into nanoseconds) cost about 70
/// instructions a reading, a few percent of a small TCP echo. Only
/// differences between readings are used, so where the clock starts (boot)
/// does not matter. It stays out of line: inlined into the worker loop it
/// moved that loop's code so that every turn on epoll cost one or two
/// instructions more, sleeping or not.
#[cfg(unix)]
#[inline(never)]
fn clock_ns() -> u64 {
    crate::sys::monotonic_ns() & !ASLEEP
}

/// Nanoseconds on a clock that is the same for every thread of the process
/// (see the Unix version).
#[cfg(not(unix))]
fn clock_ns() -> u64 {
    use std::sync::OnceLock;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let since = EPOCH.get_or_init(Instant::now).elapsed();
    u64::try_from(since.as_nanos()).unwrap_or(u64::MAX) & !ASLEEP
}

/// The counters a worker publishes for other threads (see [`Pulse`]).
///
/// Written only by the worker, with relaxed stores, on a cache line of its
/// own (but for [`idle_ns`](Self::idle_ns) of a sleep that a reader gave
/// up waiting for, see [`Counters::sleeping`]); readers may see them a
/// little late.
#[repr(align(64))]
pub(crate) struct Counters {
    ticks: AtomicU64,
    polls: AtomicU64,
    parks: AtomicU64,
    /// Nanoseconds asleep. While the worker sleeps, [`ASLEEP`] is set and the
    /// other bits hold the total minus the moment the sleep began (on
    /// [`clock_ns`], modulo 2^63): a reader adds its own clock and has the
    /// total with the sleep in progress, from one value.
    idle_ns: AtomicU64,
    spawned: AtomicU64,
    /// The location of the task being polled, or null between batches.
    polling: AtomicPtr<panic::Location<'static>>,
    /// 0 until the worker has a driver, then 1 for io_uring, 2 for epoll.
    driver: AtomicU8,
    /// Holds a sleep between its clock reading and its announcement, so a
    /// test can read the counters there. A field, not a static, so that the
    /// workers of other tests are not touched.
    #[cfg(test)]
    gap: Gap,
}

#[cfg(test)]
#[derive(Default)]
struct Gap {
    hold: std::sync::atomic::AtomicBool,
    inside: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl Gap {
    /// Waits while `hold` is set, with `inside` set meanwhile.
    fn wait(&self) {
        if !self.hold.load(Ordering::SeqCst) {
            return;
        }
        self.inside.store(true, Ordering::SeqCst);
        while self.hold.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        self.inside.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;
    use std::sync::Arc;

    /// A reading taken while the worker has its sleep's clock reading but
    /// has not stored it yet: the next reading must not count the sleep
    /// from before it. `Handle::pulse` gave 200.33 ms of idle time in a
    /// window of 200.24 ms this way on a Windows runner (tests/observe.rs).
    #[test]
    #[cfg_attr(miri, ignore = "timing: compares idle time with the clock")]
    fn idle_time_between_two_readings_never_passes_the_time_between_them() {
        let counters = Arc::new(Counters::new());
        counters.gap.hold.store(true, Ordering::SeqCst);
        let worker = {
            let counters = counters.clone();
            std::thread::spawn(move || counters.sleeping())
        };
        while !counters.gap.inside.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        // The worker has read the clock for its sleep, and that reading is
        // now well in the past.
        std::thread::sleep(Duration::from_millis(5));
        let release = {
            let counters = counters.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(5));
                counters.gap.hold.store(false, Ordering::SeqCst);
            })
        };
        let t = Instant::now();
        let before = counters.idle_ns();
        std::thread::sleep(Duration::from_millis(30));
        let after = counters.idle_ns();
        let window = t.elapsed();
        release.join().unwrap();
        worker.join().unwrap();
        let idle = Duration::from_nanos(after - before);
        assert!(idle <= window, "{idle:?} of idle time in {window:?}");
        // Most of the window, unless the release came late (a slow machine,
        // ThreadSanitizer) and the reader gave up waiting for it, which
        // `a_reader_does_not_wait_long_for_a_held_up_worker` covers.
        assert!(idle > Duration::ZERO, "{idle:?}");
    }

    /// A worker held up between its clock reading and its announcement for
    /// longer than readers wait gives the total without that sleep, rather
    /// than keeping the reader.
    #[test]
    #[cfg_attr(miri, ignore = "timing")]
    fn a_reader_does_not_wait_long_for_a_held_up_worker() {
        let counters = Arc::new(Counters::new());
        counters.gap.hold.store(true, Ordering::SeqCst);
        let worker = {
            let counters = counters.clone();
            std::thread::spawn(move || counters.sleeping())
        };
        while !counters.gap.inside.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let t = Instant::now();
        assert_eq!(counters.idle_ns(), 0);
        let gave_up = Instant::now();
        assert!(t.elapsed() < OPENING_WAIT * 10, "{:?}", t.elapsed());
        std::thread::sleep(Duration::from_millis(10));
        counters.gap.hold.store(false, Ordering::SeqCst);
        worker.join().unwrap();
        // Released, the sleep counts, but not from before the reading that
        // gave up on it: that reading said 0, and less time than has passed
        // since may be added to it. Counted from the worker's first clock
        // reading, it would be at least `OPENING_WAIT` more (CI on macOS
        // under ThreadSanitizer: 74.6 ms of idle time in 63.2 ms).
        let idle = Duration::from_nanos(counters.idle_ns());
        let since = gave_up.elapsed();
        assert!(idle > Duration::ZERO);
        assert!(idle <= since, "{idle:?} of idle time in {since:?}");
    }
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
            #[cfg(test)]
            gap: Gap::default(),
        }
    }

    pub(crate) fn set_driver(&self, kind: DriverKind) {
        let code = match kind {
            DriverKind::IoUring => 1,
            DriverKind::Epoll => 2,
            DriverKind::Kqueue => 3,
            DriverKind::Iocp => 4,
        };
        self.driver.store(code, Ordering::Relaxed);
    }

    pub(crate) fn driver(&self) -> Option<DriverKind> {
        match self.driver.load(Ordering::Relaxed) {
            1 => Some(DriverKind::IoUring),
            2 => Some(DriverKind::Epoll),
            3 => Some(DriverKind::Kqueue),
            4 => Some(DriverKind::Iocp),
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

    #[inline(always)]
    pub(crate) fn spawned(&self) {
        Self::bump(&self.spawned, 1);
    }

    /// The worker is about to sleep in the kernel. From here until
    /// [`woke`](Self::woke), readers count the sleep in progress as idle
    /// time: a worker that sleeps for minutes is idle for those minutes, not
    /// only once it wakes.
    ///
    /// The sleep begins at a clock reading that readers cannot see until it
    /// is stored. A reader in between would take the total without the
    /// sleep, and a later reading would then count it from before the
    /// first: more idle time between two readings than time passed. So
    /// [`OPENING`] goes up first, and readers wait for the reading.
    ///
    /// A reader that waits longer than [`OPENING_WAIT`] (the worker's thread
    /// was preempted in between) takes the bit away and gives the total
    /// without the sleep. The sleep then counts from a clock reading taken
    /// after that, so no later reading counts it from before.
    pub(crate) fn sleeping(&self) {
        let total = self.idle_ns.load(Ordering::Relaxed);
        // Before the clock is read: a reader that does not see it yet is
        // ahead of the reading.
        self.idle_ns.store(total | OPENING, Ordering::SeqCst);
        let now = clock_ns();
        #[cfg(test)]
        self.gap.wait();
        if self
            .idle_ns
            .compare_exchange(
                total | OPENING,
                total.wrapping_sub(now) | ASLEEP,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_err()
        {
            // A reader gave up on this sleep (see `settled`): count it from
            // a reading taken after that reader's.
            self.idle_ns
                .store(total.wrapping_sub(clock_ns()) | ASLEEP, Ordering::Release);
        }
    }

    /// The sleep announced with [`sleeping`](Self::sleeping) is over.
    pub(crate) fn woke(&self) {
        Self::bump(&self.parks, 1);
        let open = self.idle_ns.load(Ordering::Relaxed);
        self.idle_ns
            .store(open.wrapping_add(clock_ns()) & !ASLEEP, Ordering::Relaxed);
    }

    /// Nanoseconds asleep so far, the sleep in progress included.
    ///
    /// A reader's clock can run a little past the end of a sleep: the worker
    /// reads the clock, then stores the total, and a reader in between sees
    /// the sleep as still going. So a reading can be a few hundred
    /// nanoseconds more than the next one; the caller keeps the highest
    /// value it has given out (`WorkerShared::idle_seen`).
    fn idle_ns(&self) -> u64 {
        let mut value = self.settled(self.idle_ns.load(Ordering::Acquire));
        // Read the clock during the sleep the value describes, not after
        // it: the value is read again after the clock, and when it has
        // changed the worker woke (or slept again) in between. It changes
        // twice per sleep, each a system call apart, so a few tries are
        // plenty; the last one is used as it is.
        for _ in 0..4 {
            if value & ASLEEP == 0 {
                return value;
            }
            let now = clock_ns();
            let again = self.idle_ns.load(Ordering::Acquire);
            if again == value {
                return value.wrapping_add(now) & !ASLEEP;
            }
            value = self.settled(again);
        }
        if value & ASLEEP == 0 {
            value
        } else {
            value.wrapping_add(clock_ns()) & !ASLEEP
        }
    }

    /// `value`, or, while it says that a sleep is starting, what the
    /// worker stores once it has the sleep's clock reading. A worker held
    /// up past [`OPENING_WAIT`] gets the total without that sleep: the
    /// reader takes [`OPENING`] away, which tells the worker to count the
    /// sleep from a later clock reading (see [`sleeping`](Self::sleeping)).
    fn settled(&self, mut value: u64) -> u64 {
        let mut started = None;
        let mut spins = 0u32;
        while value & (ASLEEP | OPENING) == OPENING {
            if spins < 64 {
                spins += 1;
                std::hint::spin_loop();
            } else if started.get_or_insert_with(Instant::now).elapsed() < OPENING_WAIT {
                std::thread::yield_now();
            } else {
                match self.idle_ns.compare_exchange(
                    value,
                    value & !OPENING,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => return value & !OPENING,
                    // The worker stored its reading, or another reader
                    // gave up first.
                    Err(now) => {
                        value = now;
                        continue;
                    }
                }
            }
            value = self.idle_ns.load(Ordering::Acquire);
        }
        value
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

    /// Ticks, polls, parks, nanoseconds asleep, tasks spawned, and the task
    /// being polled.
    pub(crate) fn read(&self) -> (u64, u64, u64, u64, u64, Option<Location>) {
        let polling = self.polling.load(Ordering::Relaxed);
        (
            self.ticks.load(Ordering::Relaxed),
            self.polls.load(Ordering::Relaxed),
            self.parks.load(Ordering::Relaxed),
            self.idle_ns(),
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
    /// Total time spent asleep in the kernel, the sleep in progress
    /// included: for a worker with nothing to do it grows with the clock.
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
    /// The length of `sources` at which [`Local::register`] next drops the
    /// sources that are gone.
    prune_at: std::cell::Cell<usize>,
    /// Keyed by the location's value (file, line, column), not its address:
    /// one call site can have several `Location`s (Miri gives every call its
    /// own; natively, code inlined into several codegen units can).
    profile: RefCell<HashMap<Location, PollProfile>>,
    /// Requests traced on this worker (see [`take_requests`]).
    pub(crate) requests: RefCell<Requests>,
}

impl Local {
    /// Adds a source. A dropped source's `Weak` still holds its allocation,
    /// so the ones that are gone are dropped here too, whenever the list has
    /// doubled since the last time: a worker that creates and drops pools
    /// without ever taking a snapshot keeps at most twice the live ones.
    fn register(&self, source: Weak<dyn Source>) {
        let mut list = self.sources.borrow_mut();
        if list.len() >= self.prune_at.get() {
            list.retain(|s| s.strong_count() > 0);
            self.prune_at.set((list.len() * 2).max(16));
        }
        list.push(source);
    }

    /// Adds one timed poll of a task spawned at `location`.
    pub(crate) fn record(&self, location: Location, took: Duration) {
        let mut profile = self.profile.borrow_mut();
        let entry = profile.entry(location).or_insert(PollProfile {
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
        // By value: see `Local::profile`.
        let mut by_location: HashMap<Location, (Location, usize)> = HashMap::new();
        let mut tasks = 0;
        for location in w.task_locations() {
            tasks += 1;
            by_location.entry(location).or_insert((location, 0)).1 += 1;
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
    runtime::try_with_worker(|w| w.observe.register(source)).is_some()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    struct Dummy;
    impl Source for Dummy {
        fn kind(&self) -> &'static str {
            "dummy"
        }
        fn name(&self) -> String {
            String::new()
        }
        fn stats(&self) -> Vec<(&'static str, u64)> {
            Vec::new()
        }
    }

    #[test]
    fn dropped_sources_do_not_pile_up_without_snapshots() {
        let local = Local::default();
        let live: Vec<Rc<Dummy>> = (0..10).map(|_| Rc::new(Dummy)).collect();
        for source in &live {
            local.register(Rc::downgrade(source) as Weak<dyn Source>);
        }
        for _ in 0..100_000 {
            let gone = Rc::new(Dummy);
            local.register(Rc::downgrade(&gone) as Weak<dyn Source>);
        }
        let list = local.sources.borrow();
        assert!(list.len() <= 2 * live.len() + 16, "{} entries", list.len());
        assert_eq!(
            list.iter().filter(|s| s.strong_count() > 0).count(),
            live.len()
        );
    }
}
