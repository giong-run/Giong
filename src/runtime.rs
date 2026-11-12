//! Runtime construction, the per-worker event loop, and cross-thread plumbing.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::future::{poll_fn, Future};
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::panic;
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use io_uring::{opcode, types};

use crate::buf::BufPool;
use crate::driver::{Driver, Park};
use crate::error::Code;
use crate::executor::{TaskSlot, TaskWaker, MAIN_TASK};
use crate::notify::Doorbell;
use crate::op::Op;
use crate::slab::Slab;
use crate::time::Timers;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
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
}

/// The part of a worker that other threads may touch.
pub(crate) struct WorkerShared {
    /// Process-wide unique id of this worker instance (never 0).
    pub(crate) uid: usize,
    injector: Mutex<Vec<Inject>>,
    /// Tells the worker that `injector` has items, and wakes it if it sleeps.
    bell: Doorbell,
    closed: AtomicBool,
    eventfd: OwnedFd,
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
            injector: Mutex::new(Vec::new()),
            bell: Doorbell::new(),
            closed: AtomicBool::new(false),
            // SAFETY: a fresh descriptor we own.
            eventfd: unsafe { OwnedFd::from_raw_fd(fd) },
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
}

impl RuntimeShared {
    fn new(workers: usize) -> io::Result<Arc<Self>> {
        let workers = (0..workers)
            .map(|_| WorkerShared::new())
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Arc::new(Self {
            workers,
            shutdown: AtomicBool::new(false),
        }))
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
        let driver = Driver::new(cfg.ring_entries, pool.clone(), shared.eventfd.as_raw_fd())?;
        Ok(Rc::new(Self {
            index,
            shared,
            rt,
            driver: RefCell::new(driver),
            tasks: RefCell::new(Slab::new()),
            queue: RefCell::new(VecDeque::with_capacity(256)),
            timers: RefCell::new(Timers::new()),
            pool,
            next_task_uid: Cell::new(1),
            spin: cfg.spin,
            shutdown_waiters: RefCell::new(Slab::new()),
            shutdown_grace: cfg.shutdown_grace,
            draining: Cell::new(false),
        }))
    }

    /// Runs `fut` to completion on this thread, then tears the worker down.
    fn run<F: Future>(self: Rc<Self>, fut: F) -> F::Output {
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
        loop {
            self.drain_injected();
            self.run_tasks(&mut || done = poll_main());
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
            // work (see `WorkerShared::inject`).
            let mode = if self.shared.bell.prepare_park() {
                Park::Poll
            } else {
                wait
            };
            let graveyard = self.driver.borrow_mut().park(mode);
            self.shared.bell.unpark();
            // Buffers of cancelled operations: dropped outside the driver borrow.
            drop(graveyard);

            self.fire_timers();
        }
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
                if self.shared.bell.peek() || self.driver.borrow_mut().has_completions() {
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
                Inject::Run(f) => drop(panic::catch_unwind(panic::AssertUnwindSafe(f))),
                Inject::Shutdown => self.wake_shutdown_waiters(),
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

    fn config(&self) -> Config {
        Config {
            spin: self.spin,
            ring_entries: self.ring_entries,
            buf_count: self.buf_count,
            buf_size: self.buf_size,
            shutdown_grace: self.shutdown_grace,
        }
    }

    /// Creates the runtime. Nothing runs until [`Runtime::run`] is called.
    pub fn build(self) -> io::Result<Runtime> {
        let workers = self.workers.unwrap_or_else(|| allowed_cpus().len().max(1));
        let shared = RuntimeShared::new(workers)?;
        Ok(Runtime {
            builder: self,
            shared,
        })
    }

    /// Runs a single future on the current thread, using this configuration
    /// with one worker. The thread is not pinned.
    pub fn block_on<F: Future>(self, fut: F) -> io::Result<F::Output> {
        let rt = RuntimeShared::new(1)?;
        let worker = Worker::new(0, rt, &self.config())?;
        Ok(worker.run(fut))
    }
}

/// Runs a future to completion on the current thread with a single-worker
/// runtime using the default configuration.
///
/// # Panics
///
/// Panics if the runtime cannot be created (for example on a kernel without
/// the required io_uring features) or if one is already running on this thread.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    Builder::new()
        .block_on(fut)
        .expect("giong: failed to start the runtime")
}

/// A configured, not yet running, thread-per-core runtime.
pub struct Runtime {
    builder: Builder,
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
    pub fn run<F, Fut>(self, f: F) -> io::Result<()>
    where
        F: Fn(usize) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        let n = self.shared.workers.len();
        let cfg = self.builder.config();
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
                    worker.run(async move {
                        if let Some(fd) = signal_fd {
                            crate::spawn(watch_signals(fd));
                        }
                        f(index).await
                    });
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
            let sqe = opcode::Read::new(types::Fd(fd), buf.as_mut_ptr(), 8).build();
            let (res, _, b) = Op::new(sqe, buf).await;
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

    /// Runs `f` on the given worker thread and spawns the future it returns
    /// there. `f` must be `Send`; the future it creates need not be.
    ///
    /// Returns `false` if the worker index is out of range or the worker has
    /// already stopped.
    pub fn spawn_on<F, Fut>(&self, worker: usize, f: F) -> bool
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        match self.rt.workers.get(worker) {
            Some(shared) => shared.inject(Inject::Run(Box::new(move || {
                crate::spawn(f());
            }))),
            None => false,
        }
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
pub async fn until_shutdown<F: Future>(fut: F) -> Option<F::Output> {
    let mut fut = pin!(fut);
    let mut signal = pin!(shutdown_signal());
    poll_fn(|cx| {
        if signal.as_mut().poll(cx).is_ready() {
            return Poll::Ready(None);
        }
        fut.as_mut().poll(cx).map(Some)
    })
    .await
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
    /// Receive buffers currently available to the kernel.
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
