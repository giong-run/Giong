//! Runtime construction, the per-worker event loop, and cross-thread plumbing.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::panic;
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use crate::buf::BufPool;
use crate::driver::{ChosenBy, Driver, DriverChoice, DriverKind, Io, Park};
use crate::error::Code;
use crate::executor::{RemoteJoinHandle, TaskSlot, TaskWaker, MAIN_TASK};
use crate::notify::{Doorbell, JobCount};
use crate::observe::{self, Counters, Location, Pulse, WorkerState};
use crate::op::Op;
use crate::slab::Slab;
use crate::time::Timers;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

static NEXT_UID: AtomicUsize = AtomicUsize::new(1);

// ---------------------------------------------------------------- shared state

/// Work sent to a worker from another thread.
pub(crate) enum Inject {
    /// Wake the task in this slot.
    Wake(usize),
    /// Run a closure on the worker thread (used by [`Handle::spawn_on`]).
    Run(Box<dyn FnOnce() + Send>),
    /// A shutdown was requested: wake the worker's shutdown waiters.
    Shutdown,
    /// Look at the runtime's queue of balanced jobs (see
    /// [`Handle::spawn_balanced`]); nothing else to do.
    Poke,
}

/// How busy a worker is, published for other threads choosing where to
/// send work. The numbers are hints: a worker writes `queued` and `tasks`
/// between batches of tasks, with no clock reads and no ordering, and
/// readers may see them a little late. `incoming` is kept apart from them,
/// so that a worker writing its counts never erases work sent to it that it
/// has not taken in yet.
///
/// On a cache line of its own, so that a worker updating it does not slow
/// down threads that touch its injector.
#[repr(align(64))]
pub(crate) struct Load {
    /// Tasks waiting in the run queue, as of the last batch.
    queued: AtomicUsize,
    /// Live tasks, plus connections this worker kept since it last counted.
    tasks: AtomicUsize,
    /// Work other threads have sent to this worker (a `spawn_on` closure, a
    /// connection handed over by a balanced listener) that it has not taken
    /// in yet. Raised by the sender before the work is visible, lowered by
    /// the worker when it takes it.
    incoming: AtomicUsize,
}

impl Load {
    fn new() -> Self {
        Self {
            queued: AtomicUsize::new(0),
            tasks: AtomicUsize::new(0),
            incoming: AtomicUsize::new(0),
        }
    }
}

/// The part of a worker that other threads may touch.
pub(crate) struct WorkerShared {
    /// Process-wide unique id of this worker instance (never 0).
    pub(crate) uid: usize,
    pub(crate) load: Load,
    /// What the worker publishes for [`Handle::pulse`].
    pub(crate) counters: Counters,
    injector: Mutex<Vec<Inject>>,
    /// Tells the worker that `injector` has items, and wakes it if it sleeps.
    bell: Doorbell,
    closed: AtomicBool,
    eventfd: OwnedFd,
    /// How the worker's driver was chosen; set once the driver is up.
    driver_choice: OnceLock<DriverChoice>,
}

impl WorkerShared {
    fn new() -> io::Result<Arc<Self>> {
        // SAFETY: plain syscall; the result is checked.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            uid: NEXT_UID.fetch_add(1, Ordering::Relaxed),
            load: Load::new(),
            counters: Counters::new(),
            injector: Mutex::new(Vec::new()),
            bell: Doorbell::new(),
            closed: AtomicBool::new(false),
            // SAFETY: a fresh descriptor we own.
            eventfd: unsafe { OwnedFd::from_raw_fd(fd) },
            driver_choice: OnceLock::new(),
        }))
    }

    /// Hands work to the worker and wakes it if it is asleep in the kernel.
    pub(crate) fn inject(&self, item: Inject) -> bool {
        {
            let mut queue = lock(&self.injector);
            if self.closed.load(Ordering::Acquire) {
                return false;
            }
            queue.push(item);
        }
        // Handshake with `Doorbell::prepare_park` in `Worker::drive`: at least
        // one side always observes the other (modelled in `notify`).
        if self.bell.ring() {
            let one: u64 = 1;
            // SAFETY: writes 8 bytes from a valid u64 to an eventfd we own.
            unsafe {
                libc::write(
                    self.eventfd.as_raw_fd(),
                    &one as *const u64 as *const libc::c_void,
                    8,
                );
            }
        }
        true
    }

    fn close(&self) {
        let dropped = {
            let mut queue = lock(&self.injector);
            self.closed.store(true, Ordering::Release);
            mem::take(&mut *queue)
        };
        drop(dropped);
    }
}

/// State shared by all workers of one runtime.
pub(crate) struct RuntimeShared {
    workers: Vec<Arc<WorkerShared>>,
    shutdown: AtomicBool,
    /// Where the next search for the least loaded worker starts.
    next_pick: AtomicUsize,
    /// Jobs from [`Handle::spawn_balanced`] that no worker has taken yet.
    jobs: Mutex<VecDeque<Box<dyn FnOnce() + Send>>>,
    /// How many jobs wait in `jobs`, readable without the lock.
    jobs_len: JobCount,
    /// Workers time every task poll (see [`Handle::set_profiling`]).
    profiling: AtomicBool,
}

impl RuntimeShared {
    fn new(workers: usize) -> io::Result<Arc<Self>> {
        let workers = (0..workers)
            .map(|_| WorkerShared::new())
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Arc::new(Self {
            workers,
            shutdown: AtomicBool::new(false),
            next_pick: AtomicUsize::new(0),
            jobs: Mutex::new(VecDeque::new()),
            jobs_len: JobCount::new(),
            profiling: AtomicBool::new(false),
        }))
    }

    /// The worker that looks least busy: the lowest sum of live tasks,
    /// runnable tasks (so a runnable task counts twice) and work sent to it
    /// that it has not taken in yet, plus one for a worker that is awake,
    /// since it is running something. Ties go to
    /// `local` (the caller's own worker), then to the first one found from a
    /// starting point that moves on every call.
    fn least_loaded(&self, local: Option<usize>) -> usize {
        self.least_loaded_of(local, |_| true).unwrap_or(0)
    }

    /// [`least_loaded`](Self::least_loaded) among the workers for which
    /// `eligible` is true; `None` if there is none.
    pub(crate) fn least_loaded_of(
        &self,
        local: Option<usize>,
        eligible: impl Fn(usize) -> bool,
    ) -> Option<usize> {
        let n = self.workers.len();
        let start = self.next_pick.fetch_add(1, Ordering::Relaxed);
        let mut best = local.filter(|&i| i < n && eligible(i));
        let mut best_score = best.map(|i| self.score(i));
        for k in 0..n {
            let i = (start + k) % n;
            if Some(i) == local || !eligible(i) {
                continue;
            }
            let s = self.score(i);
            if best_score.map_or(true, |b| s < b) {
                best = Some(i);
                best_score = Some(s);
            }
        }
        best
    }

    /// Number of workers.
    pub(crate) fn workers(&self) -> usize {
        self.workers.len()
    }

    /// `true` while workers should time task polls.
    pub(crate) fn profiling(&self) -> bool {
        self.profiling.load(Ordering::Relaxed)
    }

    /// How busy worker `i` looks; see [`least_loaded`](Self::least_loaded).
    pub(crate) fn score(&self, i: usize) -> usize {
        let w = &self.workers[i];
        let awake = usize::from(!w.bell.looks_asleep());
        w.load.tasks.load(Ordering::Relaxed)
            + w.load.queued.load(Ordering::Relaxed)
            + w.load.incoming.load(Ordering::Relaxed)
            + awake
    }

    /// Counts work about to be sent to `worker` from any thread, before it
    /// is visible to the worker, so that the next choice in the same burst
    /// takes it into account. The worker calls [`received`](Self::received)
    /// when it takes the work in.
    pub(crate) fn sending(&self, worker: usize) {
        self.workers[worker]
            .load
            .incoming
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Worker `worker` has taken in one piece of work counted by
    /// [`sending`](Self::sending).
    pub(crate) fn received(&self, worker: usize) {
        self.workers[worker]
            .load
            .incoming
            .fetch_sub(1, Ordering::Relaxed);
    }

    /// Counts one more task on `worker`, which must be the current thread's
    /// worker, until it next writes its real count: for work it takes on
    /// several times in one batch, such as connections a balanced listener
    /// keeps.
    pub(crate) fn count_local(&self, worker: usize) {
        self.workers[worker]
            .load
            .tasks
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Queues a job for the first worker with nothing else to do, and wakes
    /// one that is asleep, if any.
    ///
    /// No wake-up is lost: see [`JobCount`] (modelled with loom in
    /// `notify`).
    fn push_job(&self, job: Box<dyn FnOnce() + Send>) {
        lock(&self.jobs).push_back(job);
        self.jobs_len.added();
        let n = self.workers.len();
        let start = self.next_pick.fetch_add(1, Ordering::Relaxed);
        for k in 0..n {
            let w = &self.workers[(start + k) % n];
            if w.bell.looks_asleep() {
                w.inject(Inject::Poke);
                return;
            }
        }
    }

    /// Takes the oldest queued balanced job, if any.
    fn take_job(&self) -> Option<Box<dyn FnOnce() + Send>> {
        if !self.jobs_len.any() {
            return None;
        }
        let job = lock(&self.jobs).pop_front();
        if job.is_some() {
            self.jobs_len.taken();
        }
        job
    }

    fn shutdown(&self) {
        if self.shutdown.swap(true, Ordering::SeqCst) {
            return;
        }
        // Each worker wakes its own waiters: they are worker-local, so that
        // waiting for a shutdown costs no shared lock on the hot path.
        for worker in &self.workers {
            worker.inject(Inject::Shutdown);
        }
    }
}

// ---------------------------------------------------------------------- worker

#[derive(Clone)]
struct Config {
    /// `None`: io_uring where the kernel allows it, epoll elsewhere.
    driver: Option<DriverKind>,
    /// Where `driver` came from.
    chosen_by: ChosenBy,
    spin: Option<Duration>,
    ring_entries: u32,
    buf_count: u16,
    buf_size: usize,
    shutdown_grace: Duration,
}

/// Everything a worker thread owns. Reached through a thread-local.
pub(crate) struct Worker {
    pub(crate) index: usize,
    pub(crate) shared: Arc<WorkerShared>,
    pub(crate) rt: Arc<RuntimeShared>,
    pub(crate) driver: RefCell<Driver>,
    /// Which driver `driver` is, readable without borrowing it.
    driver_kind: DriverKind,
    /// Descriptors given up while the driver was borrowed; closed through it
    /// at the next turn of the event loop.
    deferred_closes: RefCell<Vec<RawFd>>,
    pub(crate) tasks: RefCell<Slab<TaskSlot>>,
    pub(crate) queue: RefCell<VecDeque<usize>>,
    pub(crate) timers: RefCell<Timers>,
    pub(crate) pool: Rc<BufPool>,
    pub(crate) next_task_uid: Cell<u64>,
    /// How long to busy-poll for new work before blocking in the kernel.
    spin: Option<Duration>,
    /// Tasks on this worker waiting for a shutdown request.
    shutdown_waiters: RefCell<Slab<Waker>>,
    /// How long remaining tasks may keep running after the main future is done.
    shutdown_grace: Duration,
    /// The main future is done and the remaining tasks are being drained.
    draining: Cell<bool>,
    /// Where the main future was started (see [`crate::observe`]).
    main_location: Cell<Location>,
    /// Registered sources and the profile being gathered.
    pub(crate) observe: observe::Local,
}

thread_local! {
    static WORKER: RefCell<Option<Rc<Worker>>> = const { RefCell::new(None) };
    static CURRENT_UID: Cell<usize> = const { Cell::new(0) };
}

/// The uid of the worker running on this thread, or 0.
pub(crate) fn current_uid() -> usize {
    CURRENT_UID.try_with(|c| c.get()).unwrap_or(0)
}

/// Runs `f` with the current worker, or returns `None` outside of a runtime.
pub(crate) fn try_with_worker<R>(f: impl FnOnce(&Worker) -> R) -> Option<R> {
    WORKER
        .try_with(|w| w.borrow().as_deref().map(f))
        .ok()
        .flatten()
}

/// Runs `f` with the current worker. Panics outside of a runtime.
pub(crate) fn with_worker<R>(f: impl FnOnce(&Worker) -> R) -> R {
    try_with_worker(f).expect("giong: this must be called from inside a giong runtime")
}

impl Worker {
    fn new(index: usize, rt: Arc<RuntimeShared>, cfg: &Config) -> io::Result<Rc<Self>> {
        let shared = rt.workers[index].clone();
        let pool = BufPool::new(cfg.buf_count, cfg.buf_size)?;
        let (driver, choice) = Driver::new(
            cfg.driver,
            cfg.chosen_by,
            cfg.ring_entries,
            pool.clone(),
            shared.eventfd.as_raw_fd(),
        )?;
        shared.counters.set_driver(driver.kind());
        // A worker instance is built once, so the slot is always empty here.
        let _ = shared.driver_choice.set(choice);
        Ok(Rc::new(Self {
            index,
            shared,
            rt,
            driver_kind: driver.kind(),
            driver: RefCell::new(driver),
            deferred_closes: RefCell::new(Vec::new()),
            tasks: RefCell::new(Slab::new()),
            queue: RefCell::new(VecDeque::with_capacity(256)),
            timers: RefCell::new(Timers::new()),
            pool,
            next_task_uid: Cell::new(1),
            spin: cfg.spin,
            shutdown_waiters: RefCell::new(Slab::new()),
            shutdown_grace: cfg.shutdown_grace,
            draining: Cell::new(false),
            main_location: Cell::new(panic::Location::caller()),
            observe: observe::Local::default(),
        }))
    }

    /// Runs `fut` to completion on this thread, then tears the worker down.
    /// `location` is where the caller started the runtime.
    fn run<F: Future>(self: Rc<Self>, fut: F, location: Location) -> F::Output {
        self.main_location.set(location);
        WORKER.with(|w| {
            let mut slot = w.borrow_mut();
            assert!(
                slot.is_none(),
                "giong: a runtime is already running on this thread"
            );
            *slot = Some(self.clone());
        });
        CURRENT_UID.with(|c| c.set(self.shared.uid));

        // Tear down on every exit path, including a panic in the main future.
        struct Teardown(Rc<Worker>);
        impl Drop for Teardown {
            fn drop(&mut self) {
                self.0.teardown();
            }
        }
        let _teardown = Teardown(self.clone());

        // Declared after the guard, so the main future is dropped *before* the
        // teardown runs and its in-flight I/O is cancelled through the driver.
        let mut fut = pin!(fut);
        let main = TaskWaker::main(self.shared.clone());
        let waker = Waker::from(main.clone());
        let mut output = None;
        self.queue.borrow_mut().push_back(MAIN_TASK);

        self.drive(
            &mut || {
                if output.is_none() {
                    main.clear_queued();
                    let mut cx = Context::from_waker(&waker);
                    if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
                        output = Some(value);
                    }
                }
                output.is_some()
            },
            &|| false,
        );
        self.drain(&main, &waker);
        output.expect("giong: event loop exited before the main future completed")
    }

    /// Graceful shutdown: after the main future is done, keeps running the
    /// remaining tasks until none are left or the grace period is over.
    fn drain(&self, main: &TaskWaker, waker: &Waker) {
        if self.shutdown_grace.is_zero() || self.tasks.borrow().len() == 0 {
            return;
        }
        // From here on `shutdown_signal()` resolves on this worker, so tasks
        // know to finish what they are doing and stop taking new work.
        self.draining.set(true);
        self.wake_shutdown_waiters();

        // The deadline is an ordinary timer owned by the main task slot.
        let mut deadline = pin!(crate::time::sleep(self.shutdown_grace));
        let mut expired = false;
        self.queue.borrow_mut().push_back(MAIN_TASK);
        self.drive(
            &mut || {
                if !expired {
                    main.clear_queued();
                    let mut cx = Context::from_waker(waker);
                    expired = deadline.as_mut().poll(&mut cx).is_ready();
                }
                expired
            },
            &|| self.tasks.borrow().len() == 0,
        );
    }

    fn wake_shutdown_waiters(&self) {
        let waiters = self.shutdown_waiters.borrow_mut().take_all();
        for waker in waiters {
            waker.wake();
        }
    }

    /// The event loop: run tasks → enter the kernel → fire timers → repeat.
    ///
    /// `poll_main` polls the main future and returns `true` once it is done.
    /// `idle` is checked after every batch of tasks and also ends the loop.
    fn drive(&self, poll_main: &mut dyn FnMut() -> bool, idle: &dyn Fn() -> bool) {
        let mut done = false;
        let mut tick: u32 = 0;
        loop {
            self.shared.counters.tick();
            self.close_deferred();
            self.drain_injected();
            tick = tick.wrapping_add(1);
            // A balanced job goes to a worker with nothing else to run, and
            // now and then to a busy one, so that jobs are not left waiting
            // while every worker stays busy.
            if self.queue.borrow().is_empty() || tick % 61 == 0 {
                self.take_balanced_job();
            }
            self.publish_load();
            self.run_tasks(&mut || done = poll_main());
            self.publish_load();
            if done || idle() {
                break;
            }

            if !self.queue.borrow().is_empty() {
                // More tasks are runnable: just exchange work with the kernel.
                let graveyard = self.driver.borrow_mut().park(Park::Poll);
                drop(graveyard);
                self.fire_timers();
                continue;
            }

            if let Some(spin) = self.spin {
                // Hand queued submissions (responses!) to the kernel first, then
                // watch for new work without giving up the CPU.
                let graveyard = self.driver.borrow_mut().park(Park::Poll);
                drop(graveyard);
                self.fire_timers();
                if !self.queue.borrow().is_empty() || self.spin_for_work(spin) {
                    continue;
                }
            }

            let wait = match self.timers.borrow().next_deadline() {
                None => Park::Wait(None),
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline <= now {
                        Park::Poll
                    } else {
                        Park::Wait(Some(deadline - now))
                    }
                }
            };
            // Announce that we are going to sleep, then re-check for injected
            // work (see `WorkerShared::inject`) and balanced jobs (see
            // `RuntimeShared::push_job`): `prepare_park` ends with the fence.
            let mode = if self.shared.bell.prepare_park() || self.rt.jobs_len.any() {
                Park::Poll
            } else {
                wait
            };
            // Time spent asleep, for `Handle::pulse`: two clock reads per
            // sleep, which costs a system call anyway.
            let slept = matches!(mode, Park::Wait(_)).then(Instant::now);
            let graveyard = self.driver.borrow_mut().park(mode);
            self.shared.bell.unpark();
            if let Some(start) = slept {
                self.shared.counters.parked(start.elapsed());
            }
            // Buffers of cancelled operations: dropped outside the driver borrow.
            drop(graveyard);

            self.fire_timers();
        }
    }

    /// Hands a descriptor that was given up while the driver was borrowed to
    /// the driver at the next turn of the event loop. The epoll driver must
    /// forget a descriptor before its number can be reused, so it is never
    /// closed behind the driver's back.
    pub(crate) fn defer_close(&self, fd: RawFd) {
        self.deferred_closes.borrow_mut().push(fd);
    }

    /// Where the main future was started.
    pub(crate) fn main_location(&self) -> Location {
        self.main_location.get()
    }

    pub(crate) fn driver_kind(&self) -> DriverKind {
        self.driver_kind
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.draining.get()
    }

    /// Operations the driver tracks, and armed timers.
    pub(crate) fn io_and_timers(&self) -> (usize, usize) {
        let io = self
            .driver
            .try_borrow()
            .map(|d| d.tracked_ops())
            .unwrap_or(0);
        (io, self.timers.borrow().len())
    }

    fn close_deferred(&self) {
        if self.deferred_closes.borrow().is_empty() {
            return;
        }
        let fds = mem::take(&mut *self.deferred_closes.borrow_mut());
        let mut driver = self.driver.borrow_mut();
        for fd in fds {
            driver.close_fd(fd);
        }
    }

    /// Publishes how busy this worker is (see [`Load`]): two relaxed stores
    /// to a cache line only this thread writes.
    fn publish_load(&self) {
        let load = &self.shared.load;
        load.queued
            .store(self.queue.borrow().len(), Ordering::Relaxed);
        load.tasks
            .store(self.tasks.borrow().len(), Ordering::Relaxed);
    }

    /// Busy-polls for up to `spin`. Returns `true` as soon as there is
    /// something to do: a completion, a cross-thread wake-up or a due timer.
    fn spin_for_work(&self, spin: Duration) -> bool {
        let start = Instant::now();
        let deadline = match self.timers.borrow().next_deadline() {
            Some(timer) => timer.min(start + spin),
            None => start + spin,
        };
        loop {
            // Reading the clock costs about as much as the checks, so look at
            // the ring many times per clock read.
            for _ in 0..64 {
                if self.shared.bell.peek()
                    || self.rt.jobs_len.any()
                    || self.driver.borrow_mut().has_completions()
                {
                    return true;
                }
                std::hint::spin_loop();
            }
            if Instant::now() >= deadline {
                // A due timer is work; an expired spin budget is not.
                return self
                    .timers
                    .borrow()
                    .next_deadline()
                    .is_some_and(|timer| timer <= Instant::now());
            }
        }
    }

    /// Starts one queued balanced job on this worker, if there is one.
    fn take_balanced_job(&self) {
        if let Some(job) = self.rt.take_job() {
            // Like an injected job: a panic building the future must not
            // take the worker down.
            drop(panic::catch_unwind(panic::AssertUnwindSafe(job)));
        }
    }

    fn drain_injected(&self) {
        if !self.shared.bell.take() {
            return;
        }
        let items = mem::take(&mut *lock(&self.shared.injector));
        for item in items {
            match item {
                Inject::Wake(id) => self.queue.borrow_mut().push_back(id),
                // A panic while building the future is treated like a panic in
                // a task: it must not take the worker down.
                Inject::Run(f) => {
                    self.rt.received(self.index);
                    drop(panic::catch_unwind(panic::AssertUnwindSafe(f)));
                }
                Inject::Shutdown => self.wake_shutdown_waiters(),
                Inject::Poke => {}
            }
        }
    }

    fn fire_timers(&self) {
        if self.timers.borrow().len() == 0 {
            return;
        }
        let expired = self.timers.borrow_mut().expired(Instant::now());
        for waker in expired {
            waker.wake();
        }
    }

    fn teardown(self: &Rc<Self>) {
        // 1. Stop accepting cross-thread work.
        self.shared.close();
        // 2. Drop every task while the worker is still reachable, so that their
        //    I/O futures can hand in-flight buffers over to the driver.
        self.drop_all_tasks();
        let timers = self.timers.borrow_mut().clear();
        drop(timers);
        let waiters = self.shutdown_waiters.borrow_mut().take_all();
        drop(waiters);
        self.close_deferred();
        // 3. Cancel all I/O and wait until the kernel has let go of our memory.
        let (quiesced, graveyard) = {
            let mut driver = self.driver.borrow_mut();
            let quiesced = driver.shutdown();
            (quiesced, driver.take_graveyard())
        };
        if quiesced {
            drop(graveyard);
        } else {
            // The kernel may still write into our buffers: leak them (and the
            // ring) rather than risk a use-after-free.
            mem::forget(graveyard);
            mem::forget(self.clone());
        }
        // 4. Detach from the thread.
        let _ = WORKER.try_with(|w| w.borrow_mut().take());
        let _ = CURRENT_UID.try_with(|c| c.set(0));
    }
}

// --------------------------------------------------------------------- builder

/// Configures and starts a runtime.
///
/// ```no_run
/// giong::Builder::new()
///     .workers(4)
///     .shutdown_on_signals(true)
///     .build()?
///     .run(|worker| async move {
///         println!("worker {worker} is up");
///         giong::shutdown_signal().await;
///     })?;
/// # Ok::<(), std::io::Error>(())
/// ```
#[derive(Clone, Debug)]
pub struct Builder {
    workers: Option<usize>,
    driver: Option<DriverKind>,
    pin_cores: bool,
    ring_entries: u32,
    buf_count: u16,
    buf_size: usize,
    signals: bool,
    spin: Option<Duration>,
    shutdown_grace: Duration,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// A builder with the default configuration: one worker per available CPU,
    /// each pinned to its own core.
    pub fn new() -> Self {
        Self {
            workers: None,
            driver: None,
            pin_cores: true,
            ring_entries: 1024,
            buf_count: 512,
            buf_size: 8 * 1024,
            signals: false,
            spin: None,
            shutdown_grace: Duration::ZERO,
        }
    }

    /// Number of worker threads. Defaults to the CPUs this process may run on.
    pub fn workers(mut self, n: usize) -> Self {
        self.workers = Some(n.max(1));
        self
    }

    /// The I/O driver every worker uses. By default giong uses io_uring where
    /// the kernel allows it and falls back to epoll where it does not: in a
    /// Docker container with the default seccomp profile, under gVisor, with
    /// the `kernel.io_uring_disabled` sysctl set, or on a kernel older than
    /// 6.0. [`driver_kind`](crate::driver_kind) tells which one a worker got.
    ///
    /// Name a driver to take that choice away: with
    /// [`DriverKind::IoUring`] the runtime fails to start where io_uring is
    /// not available (`GIONG-E0001`) instead of running on epoll; with
    /// [`DriverKind::Epoll`] it never uses io_uring.
    ///
    /// When no driver is named here, the `GIONG_DRIVER` environment variable
    /// can name one (`io_uring` or `epoll`), so that a deployment can choose
    /// without a rebuild. Any other value makes [`build`](Self::build) fail
    /// with `GIONG-E0004`.
    ///
    /// ```no_run
    /// let rt = giong::Builder::new().driver(giong::DriverKind::Epoll).build()?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn driver(mut self, kind: DriverKind) -> Self {
        self.driver = Some(kind);
        self
    }

    /// Pin each worker thread to one CPU (default: `true`).
    pub fn pin_cores(mut self, pin: bool) -> Self {
        self.pin_cores = pin;
        self
    }

    /// Size of each worker's io_uring submission queue (default: 1024).
    pub fn ring_entries(mut self, entries: u32) -> Self {
        self.ring_entries = entries.clamp(8, 32768).next_power_of_two();
        self
    }

    /// Per-worker receive buffer pool: `count` buffers (rounded up to a power
    /// of two, at most 32768) of `size` bytes each. Default: 512 × 8 KiB.
    ///
    /// The pool only needs to cover the connections that receive data within
    /// one turn of the event loop; when it runs short, giong falls back to
    /// heap copies instead of failing.
    ///
    /// `count` is an upper bound, not what a worker uses. The memory is
    /// reserved, not touched; free buffers are reused most recent first, and
    /// with io_uring a worker lends the kernel 16 of them at first and twice
    /// as many each time a turn of the event loop takes most of those. A
    /// worker serving a few connections touches a few buffers.
    pub fn recv_buffers(mut self, count: usize, size: usize) -> Self {
        self.buf_count = count.clamp(1, 32768).next_power_of_two() as u16;
        self.buf_size = size.max(64);
        self
    }

    /// Busy-poll for up to `duration` before a worker goes to sleep in the
    /// kernel (default: never).
    ///
    /// A sleeping worker has to be woken by an interrupt and rescheduled
    /// before it can react, which costs tens of microseconds — more inside a
    /// virtual machine. While spinning, the worker notices new completions
    /// within nanoseconds. This trades CPU for latency: a spinning worker
    /// keeps its core busy even when there is nothing to do, so use it for
    /// latency-sensitive services on dedicated cores, with a duration on the
    /// order of the gaps between requests (tens to hundreds of microseconds).
    pub fn spin_before_park(mut self, duration: Duration) -> Self {
        self.spin = (!duration.is_zero()).then_some(duration);
        self
    }

    /// Request a runtime shutdown when the process receives SIGINT or SIGTERM
    /// (default: `false`). Only applies to [`Runtime::run`].
    pub fn shutdown_on_signals(mut self, enable: bool) -> Self {
        self.signals = enable;
        self
    }

    /// Graceful shutdown: once a worker's main future has completed, keep
    /// running that worker's remaining tasks for up to `grace`, so they can
    /// finish what they are doing. Tasks still alive after that are dropped.
    /// Default: zero, which drops the remaining tasks at once.
    ///
    /// While a worker drains, [`shutdown_signal`] and [`until_shutdown`]
    /// resolve on it, which is how a task learns that it should wrap up:
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn handle(_: &giong::net::TcpStream, _: &[u8]) {}
    /// giong::Builder::new()
    ///     .shutdown_on_signals(true)
    ///     .shutdown_grace(Duration::from_secs(10))
    ///     .build()?
    ///     .run(|_worker| async {
    ///         let listener = giong::net::TcpListener::bind("0.0.0.0:8080").expect("bind");
    ///         // Stop accepting as soon as a shutdown is requested...
    ///         while let Some(Ok(stream)) = giong::until_shutdown(listener.accept()).await {
    ///             giong::spawn(async move {
    ///                 // ...and let each connection finish the request it is
    ///                 // working on, then close instead of waiting for another.
    ///                 while let Some(Ok(data)) = giong::until_shutdown(stream.recv()).await {
    ///                     if data.is_empty() {
    ///                         return;
    ///                     }
    ///                     handle(&stream, &data).await;
    ///                 }
    ///             });
    ///         }
    ///     })?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }

    /// The settings each worker starts from. Reads `GIONG_DRIVER` when no
    /// driver was named.
    fn config(&self) -> io::Result<Config> {
        let (driver, chosen_by) = match self.driver {
            Some(kind) => (Some(kind), ChosenBy::Builder),
            None => match crate::driver::kind_from_env()? {
                Some(kind) => (Some(kind), ChosenBy::Environment),
                None => (None, ChosenBy::Automatic),
            },
        };
        Ok(Config {
            driver,
            chosen_by,
            spin: self.spin,
            ring_entries: self.ring_entries,
            buf_count: self.buf_count,
            buf_size: self.buf_size,
            shutdown_grace: self.shutdown_grace,
        })
    }

    /// Creates the runtime. Nothing runs until [`Runtime::run`] is called.
    pub fn build(self) -> io::Result<Runtime> {
        let cfg = self.config()?;
        let workers = self.workers.unwrap_or_else(|| allowed_cpus().len().max(1));
        let shared = RuntimeShared::new(workers)?;
        Ok(Runtime {
            builder: self,
            cfg,
            shared,
        })
    }

    /// Runs a single future on the current thread, using this configuration
    /// with one worker. The thread is not pinned.
    #[track_caller]
    pub fn block_on<F: Future>(self, fut: F) -> io::Result<F::Output> {
        let location = panic::Location::caller();
        let cfg = self.config()?;
        let rt = RuntimeShared::new(1)?;
        let worker = Worker::new(0, rt, &cfg)?;
        Ok(worker.run(fut, location))
    }
}

/// Runs a future to completion on the current thread with a single-worker
/// runtime using the default configuration.
///
/// # Panics
///
/// Panics if the runtime cannot be created (for example on a kernel without
/// the required io_uring features) or if one is already running on this thread.
#[track_caller]
pub fn block_on<F: Future>(fut: F) -> F::Output {
    Builder::new()
        .block_on(fut)
        .expect("giong: failed to start the runtime")
}

/// A configured, not yet running, thread-per-core runtime.
pub struct Runtime {
    builder: Builder,
    cfg: Config,
    shared: Arc<RuntimeShared>,
}

/// Lets all workers start together, or not at all.
struct Gate {
    state: Mutex<Option<bool>>,
    cond: Condvar,
}

impl Gate {
    fn open(&self, go: bool) {
        *lock(&self.state) = Some(go);
        self.cond.notify_all();
    }

    fn wait(&self) -> bool {
        let mut state = lock(&self.state);
        loop {
            if let Some(go) = *state {
                return go;
            }
            state = self.cond.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Runtime {
    /// A handle that can be sent to other threads to talk to this runtime.
    pub fn handle(&self) -> Handle {
        Handle {
            rt: self.shared.clone(),
        }
    }

    /// Starts one thread per worker and blocks until all of them finish.
    ///
    /// `f` is called once on each worker thread (with the worker's index) to
    /// build that worker's main future; a worker finishes when its main future
    /// completes, at which point its remaining tasks are dropped (after the
    /// grace period set with [`Builder::shutdown_grace`], if any). Because each
    /// future is created on the thread that runs it, it may be `!Send`.
    ///
    /// A panic in a worker's main future shuts the runtime down and is
    /// propagated to the caller.
    #[track_caller]
    pub fn run<F, Fut>(self, f: F) -> io::Result<()>
    where
        F: Fn(usize) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let location = panic::Location::caller();
        let n = self.shared.workers.len();
        let cfg = self.cfg.clone();
        let cpus = allowed_cpus();
        let f = Arc::new(f);
        let gate = Arc::new(Gate {
            state: Mutex::new(None),
            cond: Condvar::new(),
        });
        let signals = if self.builder.signals {
            Some(SignalGuard::install()?)
        } else {
            None
        };
        let signal_fd = signals.as_ref().map(|s| s.fd);

        let (tx, rx) = mpsc::channel::<io::Result<()>>();
        let mut threads = Vec::with_capacity(n);
        let mut spawn_err = None;
        for index in 0..n {
            let (f, gate, tx, cfg) = (f.clone(), gate.clone(), tx.clone(), cfg.clone());
            let rt = self.shared.clone();
            let cpu =
                (self.builder.pin_cores && !cpus.is_empty()).then(|| cpus[index % cpus.len()]);
            let signal_fd = if index == 0 { signal_fd } else { None };
            let spawned = thread::Builder::new()
                .name(format!("giong-{index}"))
                .spawn(move || {
                    if let Some(cpu) = cpu {
                        pin_to_cpu(cpu);
                    }
                    let worker = match Worker::new(index, rt.clone(), &cfg) {
                        Ok(worker) => {
                            let _ = tx.send(Ok(()));
                            worker
                        }
                        Err(e) => {
                            let _ = tx.send(Err(e));
                            return;
                        }
                    };
                    drop(tx);
                    if !gate.wait() {
                        return;
                    }
                    // If this worker dies, tell the others to stop.
                    struct StopOnPanic(Arc<RuntimeShared>);
                    impl Drop for StopOnPanic {
                        fn drop(&mut self) {
                            if thread::panicking() {
                                self.0.shutdown();
                            }
                        }
                    }
                    let _stop = StopOnPanic(rt);
                    worker.run(
                        async move {
                            if let Some(fd) = signal_fd {
                                crate::spawn(watch_signals(fd));
                            }
                            f(index).await
                        },
                        location,
                    );
                });
            match spawned {
                Ok(t) => threads.push(t),
                Err(e) => {
                    spawn_err = Some(e);
                    break;
                }
            }
        }
        drop(tx);

        let mut first_err = spawn_err;
        for _ in 0..threads.len() {
            match rx.recv() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    first_err.get_or_insert(e);
                }
                Err(_) => break,
            }
        }
        gate.open(first_err.is_none());

        let mut panic_payload = None;
        for t in threads {
            if let Err(payload) = t.join() {
                panic_payload.get_or_insert(payload);
            }
        }
        drop(signals);

        if let Some(e) = first_err {
            return Err(e);
        }
        if let Some(payload) = panic_payload {
            panic::resume_unwind(payload);
        }
        Ok(())
    }
}

// --------------------------------------------------------------------- signals

/// The eventfd the signal handler writes to. Created once per process and
/// never closed, so the handler can never race with a close.
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);
/// Set while a runtime has the handlers installed.
static SIGNALS_IN_USE: AtomicBool = AtomicBool::new(false);

/// Async-signal-safe: only `write(2)`, with `errno` preserved.
extern "C" fn on_signal(_signal: libc::c_int) {
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let one: u64 = 1;
        // SAFETY: `write` is async-signal-safe; `errno` is restored afterwards.
        unsafe {
            let errno = libc::__errno_location();
            let saved = *errno;
            libc::write(fd, &one as *const u64 as *const libc::c_void, 8);
            *errno = saved;
        }
    }
}

/// Installs SIGINT/SIGTERM handlers that poke an eventfd; restores the
/// previous handlers when dropped.
///
/// Handlers (rather than a blocked-signal `signalfd`) are used because signal
/// masks are per thread: a `signalfd` only works if *every* thread of the
/// process blocks the signals, which a library cannot guarantee.
struct SignalGuard {
    fd: RawFd,
    old_int: libc::sigaction,
    old_term: libc::sigaction,
}

impl SignalGuard {
    fn install() -> io::Result<Self> {
        if SIGNALS_IN_USE.swap(true, Ordering::SeqCst) {
            return Err(crate::error::raise(
                Code::SignalsTaken,
                "another giong runtime in this process already handles SIGINT and SIGTERM",
                "turn on Builder::shutdown_on_signals for one runtime only, or run one runtime \
                 at a time.",
            ));
        }
        // SAFETY: plain libc calls with valid arguments; results are checked.
        unsafe {
            let mut fd = SIGNAL_FD.load(Ordering::SeqCst);
            if fd < 0 {
                fd = libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK);
                if fd < 0 {
                    SIGNALS_IN_USE.store(false, Ordering::SeqCst);
                    return Err(io::Error::last_os_error());
                }
                SIGNAL_FD.store(fd, Ordering::SeqCst);
            }
            // Forget signals that arrived while nobody was listening.
            let mut stale: u64 = 0;
            libc::read(fd, &mut stale as *mut u64 as *mut libc::c_void, 8);

            let mut action: libc::sigaction = mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut old_int: libc::sigaction = mem::zeroed();
            let mut old_term: libc::sigaction = mem::zeroed();
            if libc::sigaction(libc::SIGINT, &action, &mut old_int) != 0 {
                let err = io::Error::last_os_error();
                SIGNALS_IN_USE.store(false, Ordering::SeqCst);
                return Err(err);
            }
            if libc::sigaction(libc::SIGTERM, &action, &mut old_term) != 0 {
                let err = io::Error::last_os_error();
                libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut());
                SIGNALS_IN_USE.store(false, Ordering::SeqCst);
                return Err(err);
            }
            Ok(Self {
                fd,
                old_int,
                old_term,
            })
        }
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        // SAFETY: restores the dispositions saved in `install`.
        unsafe {
            libc::sigaction(libc::SIGINT, &self.old_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.old_term, std::ptr::null_mut());
        }
        SIGNALS_IN_USE.store(false, Ordering::SeqCst);
    }
}

/// Waits for the signal handler to poke the eventfd, then requests shutdown.
///
/// Ends as soon as a shutdown is requested by any other means, so that it never
/// keeps a draining worker alive.
async fn watch_signals(fd: RawFd) {
    until_shutdown(async {
        let mut buf: Vec<u8> = Vec::with_capacity(8);
        loop {
            let io = Io::ReadReady {
                fd,
                buf: buf.as_mut_ptr(),
                len: 8,
            };
            let (res, _, b) = Op::new(io, buf).await;
            buf = b;
            if res > 0 {
                handle().shutdown();
                return;
            }
            if res != -libc::EINTR && res != -libc::EAGAIN {
                return;
            }
        }
    })
    .await;
}

// ---------------------------------------------------------------------- handle

/// A cloneable, thread-safe handle to a runtime.
#[derive(Clone)]
pub struct Handle {
    rt: Arc<RuntimeShared>,
}

impl Handle {
    /// Number of workers in the runtime.
    pub fn workers(&self) -> usize {
        self.rt.workers.len()
    }

    /// Asks the runtime to shut down: every [`shutdown_signal`] future resolves.
    /// It is up to each worker's main future to return in response.
    pub fn shutdown(&self) {
        self.rt.shutdown();
    }

    /// `true` once a shutdown has been requested.
    pub fn is_shutdown(&self) -> bool {
        self.rt.shutdown.load(Ordering::SeqCst)
    }

    /// Starts a task on the given worker and returns a handle to its result.
    ///
    /// `f` is sent to the worker and called there. The future it returns runs
    /// on that worker and never leaves it, so it need not be `Send`; only `f`
    /// and the output cross threads. Await the handle from any worker or
    /// thread, or drop it to let the task run on its own.
    ///
    /// The handle resolves to [`JoinError::Cancelled`] when the task is
    /// aborted, or when the worker has stopped or stops before the task
    /// finishes. It resolves to [`JoinError::Panic`] when `f` or the future
    /// panics; the worker carries on.
    ///
    /// ```no_run
    /// let rt = giong::Builder::new().workers(2).build()?;
    /// let handle = rt.handle();
    /// rt.run(move |worker| {
    ///     let handle = handle.clone();
    ///     async move {
    ///         if worker == 0 {
    ///             // Runs on worker 1; the result comes back here.
    ///             let id = handle.spawn_on(1, || async { giong::worker_id() }).await;
    ///             assert_eq!(id.unwrap(), 1);
    ///             handle.shutdown();
    ///         }
    ///         giong::shutdown_signal().await;
    ///     }
    /// })?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    ///
    /// # Panics
    ///
    /// Panics with [`Code::NoSuchWorker`] if `worker` is not below
    /// [`workers`](Self::workers).
    ///
    /// [`JoinError::Cancelled`]: crate::JoinError::Cancelled
    /// [`JoinError::Panic`]: crate::JoinError::Panic
    #[track_caller]
    pub fn spawn_on<F, Fut>(&self, worker: usize, f: F) -> RemoteJoinHandle<Fut::Output>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future + 'static,
        Fut::Output: Send + 'static,
    {
        let Some(shared) = self.rt.workers.get(worker) else {
            let workers = self.workers();
            panic!(
                "{}",
                crate::error::Error::new(
                    Code::NoSuchWorker,
                    format!("spawn_on was given worker {worker}, but the runtime has {workers} (0 to {})", workers.saturating_sub(1)),
                    "pass an index below Handle::workers(), such as `i % handle.workers()`.",
                )
            );
        };
        let (handle, job) = RemoteJoinHandle::new(f, panic::Location::caller());
        self.rt.sending(worker);
        // A stopped worker drops the job, which cancels the handle.
        shared.inject(Inject::Run(job));
        handle
    }

    /// Starts a task on the first worker that has nothing else to run, and
    /// returns a handle to its result.
    ///
    /// Use it for work that may run anywhere, such as CPU-heavy jobs a
    /// request needs done: they go to idle workers rather than queueing
    /// behind the caller's other tasks. `f` waits in a queue shared by the
    /// runtime's workers; a worker whose own run queue is empty takes the
    /// next one, calls it, and runs the future it returns. An idle worker
    /// is woken for it. Busy workers also take one now and then, so a job
    /// is never stuck while every worker stays busy. Jobs start in the order
    /// they were queued.
    ///
    /// Only the start is balanced: the future stays on the worker that took
    /// `f` (giong never moves a running task, so tasks need not be `Send`).
    /// Otherwise it behaves like [`spawn_on`](Self::spawn_on): only `f` and
    /// the output cross threads, and the handle reports a panic or a
    /// cancellation the same way. Jobs still queued when the runtime stops
    /// are dropped, which cancels their handles.
    ///
    /// ```no_run
    /// let rt = giong::Builder::new().workers(4).build()?;
    /// let handle = rt.handle();
    /// rt.run(move |worker| {
    ///     let handle = handle.clone();
    ///     async move {
    ///         if worker == 0 {
    ///             let jobs: Vec<_> = (0..8u64)
    ///                 .map(|n| handle.spawn_balanced(move || async move { (0..n * 1_000_000).sum::<u64>() }))
    ///                 .collect();
    ///             for job in jobs {
    ///                 job.await.unwrap();
    ///             }
    ///             handle.shutdown();
    ///         }
    ///         giong::shutdown_signal().await;
    ///     }
    /// })?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    #[track_caller]
    pub fn spawn_balanced<F, Fut>(&self, f: F) -> RemoteJoinHandle<Fut::Output>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future + 'static,
        Fut::Output: Send + 'static,
    {
        let (handle, job) = RemoteJoinHandle::new(f, panic::Location::caller());
        self.rt.push_job(job);
        handle
    }

    /// The index of the worker that looks least busy, for code that places
    /// work itself, for example handing a new connection to another worker
    /// with [`spawn_on`](Self::spawn_on).
    ///
    /// The score is the worker's live tasks plus its runnable ones and the
    /// work sent to it that it has not started yet, plus one if it is awake
    /// (it is running something); the lowest wins, and ties
    /// favour the caller's own worker, then rotate. Each worker publishes
    /// these numbers between batches of tasks, so they can be slightly out
    /// of date. Calling this changes nothing; sending the work with
    /// [`spawn_on`](Self::spawn_on) counts it on the chosen worker at once,
    /// so that the next choice takes it into account.
    pub fn least_loaded_worker(&self) -> usize {
        self.rt.least_loaded(self.local_index())
    }

    /// How worker `worker` got its I/O driver (see [`DriverChoice`]), read
    /// from any thread; `None` until the worker has started.
    ///
    /// # Panics
    ///
    /// Panics with [`Code::NoSuchWorker`] if `worker` is not below
    /// [`workers`](Self::workers).
    pub fn driver_choice(&self, worker: usize) -> Option<DriverChoice> {
        let Some(shared) = self.rt.workers.get(worker) else {
            let workers = self.workers();
            panic!(
                "{}",
                crate::error::Error::new(
                    Code::NoSuchWorker,
                    format!(
                        "driver_choice was given worker {worker}, but the runtime has {workers} \
                         (0 to {})",
                        workers.saturating_sub(1)
                    ),
                    "pass an index below Handle::workers().",
                )
            );
        };
        shared.driver_choice.get().cloned()
    }

    /// What worker `worker` is doing and has done, read from any thread
    /// without disturbing it: see [`observe::Pulse`].
    ///
    /// # Panics
    ///
    /// Panics with [`Code::NoSuchWorker`] if `worker` is not below
    /// [`workers`](Self::workers).
    pub fn pulse(&self, worker: usize) -> Pulse {
        let Some(shared) = self.rt.workers.get(worker) else {
            let workers = self.workers();
            panic!(
                "{}",
                crate::error::Error::new(
                    Code::NoSuchWorker,
                    format!(
                        "pulse was given worker {worker}, but the runtime has {workers} (0 to {})",
                        workers.saturating_sub(1)
                    ),
                    "pass an index below Handle::workers().",
                )
            );
        };
        let (ticks, polls, parks, idle, spawned, polling) = shared.counters.read();
        let state = if shared.closed.load(Ordering::Acquire) || ticks == 0 {
            WorkerState::Stopped
        } else if shared.bell.looks_asleep() {
            WorkerState::Parked
        } else {
            WorkerState::Running
        };
        Pulse {
            worker,
            state,
            driver: shared.counters.driver(),
            ticks,
            polls,
            parks,
            idle,
            spawned,
            tasks: shared.load.tasks.load(Ordering::Relaxed),
            runnable: shared.load.queued.load(Ordering::Relaxed),
            polling: if state == WorkerState::Running {
                polling
            } else {
                None
            },
        }
    }

    /// Turns the timing of task polls on or off on every worker. While it is
    /// on, each worker reads the clock around every poll (two reads, some
    /// tens of nanoseconds) and adds the time to the place the task was
    /// spawned; [`observe::take_profile`] collects the result on a worker.
    /// Workers notice the change at their next batch of tasks.
    pub fn set_profiling(&self, on: bool) {
        self.rt.profiling.store(on, Ordering::Relaxed);
    }

    /// `true` while task polls are being timed (see
    /// [`set_profiling`](Self::set_profiling)).
    pub fn is_profiling(&self) -> bool {
        self.rt.profiling()
    }

    /// The caller's worker index, if it runs on a worker of this runtime.
    fn local_index(&self) -> Option<usize> {
        try_with_worker(|w| Arc::ptr_eq(&w.rt, &self.rt).then_some(w.index)).flatten()
    }
}

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("workers", &self.workers())
            .finish()
    }
}

/// A handle to the runtime the current task is running on.
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn handle() -> Handle {
    with_worker(|w| Handle { rt: w.rt.clone() })
}

/// Index of the worker the current task is running on (`0..workers`).
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn worker_id() -> usize {
    with_worker(|w| w.index)
}

/// The I/O driver of the worker the current task is running on: io_uring,
/// or epoll where io_uring is not available (see [`Builder::driver`]).
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn driver_kind() -> DriverKind {
    with_worker(|w| w.driver_kind)
}

/// How the worker the current task is running on got its I/O driver: which
/// one, who chose it, why io_uring is not used if giong fell back to epoll,
/// and the kernel bugs giong works around. Its `Display` form is one line,
/// for a startup log:
///
/// ```no_run
/// giong::block_on(async {
///     println!("giong driver: {}", giong::driver_choice());
/// });
/// ```
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn driver_choice() -> DriverChoice {
    with_worker(|w| {
        w.shared
            .driver_choice
            .get()
            .cloned()
            .expect("giong: a running worker has a driver")
    })
}

/// Index of the worker the current thread runs, or `None` outside of a giong
/// runtime. For code that may also run elsewhere, such as a log writer.
pub fn try_worker_id() -> Option<usize> {
    try_with_worker(|w| w.index)
}

/// Future returned by [`shutdown_signal`].
pub struct ShutdownSignal {
    rt: Arc<RuntimeShared>,
    /// Registration in the worker's waiter list, and that worker's uid.
    key: Option<(usize, usize)>,
    /// Registered with one worker thread.
    _not_send: PhantomData<*const ()>,
}

impl ShutdownSignal {
    fn unregister(&mut self) {
        if let Some((key, uid)) = self.key.take() {
            // After a shutdown the waiter list has been emptied and the key is stale.
            if self.rt.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let waker = try_with_worker(|w| {
                if w.shared.uid == uid && !w.draining.get() {
                    w.shutdown_waiters
                        .try_borrow_mut()
                        .ok()
                        .and_then(|mut waiters| waiters.remove(key))
                } else {
                    None
                }
            });
            drop(waker);
        }
    }
}

impl Future for ShutdownSignal {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.rt.shutdown.load(Ordering::SeqCst) || with_worker(|w| w.draining.get()) {
            self.key = None;
            return Poll::Ready(());
        }
        // The flag is set *before* the wake-up is injected into this worker,
        // and this thread drains injected work only after we return, so a
        // registration made here cannot miss it.
        let registered = self.key;
        let key = with_worker(|w| {
            let uid = w.shared.uid;
            let mut waiters = w.shutdown_waiters.borrow_mut();
            if let Some((key, owner)) = registered {
                if owner == uid {
                    if let Some(waker) = waiters.get_mut(key) {
                        if !waker.will_wake(cx.waker()) {
                            *waker = cx.waker().clone();
                        }
                        return (key, uid);
                    }
                }
            }
            (waiters.insert(cx.waker().clone()), uid)
        });
        self.key = Some(key);
        Poll::Pending
    }
}

impl Drop for ShutdownSignal {
    fn drop(&mut self) {
        self.unregister();
    }
}

/// Resolves once a shutdown of the current runtime has been requested, via
/// [`Handle::shutdown`] or a signal (see [`Builder::shutdown_on_signals`]), or
/// once the current worker's main future has completed and the worker is
/// draining its remaining tasks (see [`Builder::shutdown_grace`]).
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn shutdown_signal() -> ShutdownSignal {
    ShutdownSignal {
        rt: with_worker(|w| w.rt.clone()),
        key: None,
        _not_send: PhantomData,
    }
}

/// `true` once [`shutdown_signal`] would resolve on the current worker: a
/// shutdown has been requested, or the worker is draining its remaining tasks.
/// A cheap check for loops that are busy rather than waiting.
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn is_shutting_down() -> bool {
    with_worker(|w| w.rt.shutdown.load(Ordering::Relaxed) || w.draining.get())
}

/// Runs `fut` until it completes or a shutdown is requested, whichever comes
/// first. Returns `None` if the shutdown won.
///
/// ```no_run
/// # async fn demo(listener: giong::net::TcpListener) {
/// while let Some(conn) = giong::until_shutdown(listener.accept()).await {
///     // handle `conn`
/// }
/// # }
/// ```
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
pub fn until_shutdown<F: Future>(fut: F) -> UntilShutdown<F> {
    UntilShutdown {
        fut,
        signal: shutdown_signal(),
    }
}

/// Future returned by [`until_shutdown`]: `Some` with the output of the
/// future it wraps, or `None` if a shutdown was requested first. Only as
/// large as the wrapped future plus a [`ShutdownSignal`].
#[must_use = "futures do nothing unless awaited"]
pub struct UntilShutdown<F> {
    /// Pinned with the `UntilShutdown`: never moved out.
    fut: F,
    signal: ShutdownSignal,
}

impl<F: Future> Future for UntilShutdown<F> {
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `fut` is structurally pinned: it is only reached through
        // the pinned reference made here and never moved. `ShutdownSignal`
        // is `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        if Pin::new(&mut this.signal).poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        unsafe { Pin::new_unchecked(&mut this.fut) }
            .poll(cx)
            .map(Some)
    }
}

impl<F> std::fmt::Debug for UntilShutdown<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UntilShutdown").finish_non_exhaustive()
    }
}

// ------------------------------------------------------------------- debugging

/// A snapshot of the current worker's internals, for tests and diagnostics.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    /// Live tasks, including the caller.
    pub tasks: usize,
    /// Operations tracked by the driver (in flight or awaiting collection).
    pub tracked_ops: usize,
    /// SQEs whose final completion has not been seen yet.
    pub inflight: usize,
    /// Receive buffers not in use (lent to the kernel, or spare).
    pub free_recv_buffers: usize,
    /// Size of the receive buffer pool.
    pub recv_buffers: usize,
    /// Armed timers.
    pub timers: usize,
}

#[doc(hidden)]
pub fn stats() -> Stats {
    with_worker(|w| {
        let driver = w.driver.borrow();
        Stats {
            tasks: w.tasks.borrow().len(),
            tracked_ops: driver.tracked_ops(),
            inflight: driver.inflight(),
            free_recv_buffers: w.pool.free(),
            recv_buffers: w.pool.count() as usize,
            timers: w.timers.borrow().len(),
        }
    })
}

// -------------------------------------------------------------------- affinity

/// CPUs the calling thread is allowed to run on.
fn allowed_cpus() -> Vec<usize> {
    // SAFETY: `set` is a valid, zeroed cpu_set_t of the size we pass.
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        if libc::sched_getaffinity(0, mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            let n = thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            return (0..n).collect();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| libc::CPU_ISSET(cpu, &set))
            .collect()
    }
}

fn pin_to_cpu(cpu: usize) {
    // SAFETY: as above. Failure is not fatal: the worker just stays unpinned.
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set);
    }
}
