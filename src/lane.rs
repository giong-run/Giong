//! The blocking lane and the compute lane: where work goes that must not run
//! on a worker.
//!
//! A worker runs every task on its core, between turns of its event loop. A
//! task that blocks the thread (a synchronous library call, `std::fs`, a C
//! library, a DNS lookup through libc) or computes for a long time (password
//! hashing, image resizing, compression, JSON of many megabytes) holds up
//! every other task and every connection of that worker for as long as it
//! runs. Such work goes to one of two lanes instead, each a pool of plain
//! threads next to the workers:
//!
//! | Lane | For | Threads | Priority |
//! |---|---|---|---|
//! | [`spawn_blocking`] | Work that **waits**: blocking system calls and libraries | Grow on demand up to [`Builder::max_blocking_threads`](crate::Builder::max_blocking_threads) (default 128), end after 10 s idle | Normal |
//! | [`spawn_compute`] | Work that **computes**: CPU-bound jobs | Up to [`Builder::compute_threads`](crate::Builder::compute_threads), by default the CPU budget of the process (its CPUs, or its cgroup CPU quota) | Lower than the workers (nice +10; the utility QoS class on macOS) |
//!
//! Both return a [`BlockingHandle`]; awaiting it gives the closure's value,
//! or a [`JoinError`] if the closure panicked or never ran.
//!
//! ```no_run
//! # fn hash_password(_: &str) -> String { String::new() }
//! # fn read_config() -> std::io::Result<String> { Ok(String::new()) }
//! giong::block_on(async {
//!     // Waits on the disk with a plain `std::fs` call: the blocking lane.
//!     let config = giong::spawn_blocking(read_config).await.unwrap()?;
//!
//!     // Burns CPU for tens of milliseconds: the compute lane.
//!     let hash = giong::spawn_compute(|| hash_password("hunter2")).await.unwrap();
//!     # let _ = (config, hash);
//!     Ok::<(), std::io::Error>(())
//! })?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! ## Why the compute lane runs at a lower priority
//!
//! By default the workers already take every CPU the process may use, so a
//! compute thread always shares a core with a worker. The compute threads
//! run at nice +10 (on macOS, where the nice value is the whole process's,
//! in the utility quality-of-service class), which the kernel's scheduler
//! weighs at about a tenth of a worker: while a worker has work it gets its core back within the
//! scheduler's time slice, and while the workers are idle the compute lane
//! uses all of the CPU. Compute jobs still progress under full network load,
//! more slowly. `bench/lanes.sh` measures what this buys; its results are in
//! `bench/results/lanes/`.
//!
//! Under a cgroup CPU quota (a container's CPU limit), every thread of the
//! process draws on the same quota, whatever its priority: a busy compute
//! lane can use up the quota and the kernel then stops the workers too until
//! the next period. Where the workers need their full share, give the lane
//! its own CPUs: fewer [`workers`](crate::Builder::workers) and a matching
//! [`compute_threads`](crate::Builder::compute_threads).
//!
//! ## Overload
//!
//! [`spawn_compute`] queues without limit, like [`spawn`](crate::spawn).
//! [`try_spawn_compute`] refuses instead, with
//! [`GIONG-E0014`](crate::error::Code::ComputeBusy), once
//! [`compute_backlog`](crate::Builder::compute_backlog) jobs already wait
//! for a thread: a server answers 503 rather than queue work it cannot do
//! in time.
//!
//! ## Tenants
//!
//! Closures queued from a task of a [`Budget`](crate::budget::Budget), or
//! with [`Budget::spawn_compute`](crate::budget::Budget::spawn_compute), wait
//! in that budget's own queue on the compute lane. A free compute thread
//! takes the next closure of the budget that has used the least compute time
//! for its weight, so one tenant's long jobs do not make another tenant's
//! short jobs wait behind them. Closures outside any budget share one queue,
//! served the same way. See [`crate::budget`].
//!
//! ## What a lane does not do
//!
//! * A closure that has started runs to its end: [`BlockingHandle::abort`]
//!   and dropping the runtime only stop closures that have not started.
//!   Dropping a handle lets its closure run on its own.
//! * Closures run on plain threads: they cannot use giong's I/O, timers or
//!   [`spawn`](crate::spawn) (which panic there with `GIONG-E0011`). To send
//!   results back as they come, use a channel from [`crate::sync`].
//! * When [`Runtime::run`](crate::Runtime::run) returns it does not wait for
//!   running closures; they finish on their threads.

use std::fmt;
use std::future::Future;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use crate::budget::ComputeCharge;
use crate::error::{raise, Code};
use crate::executor::JoinError;
pub use crate::pool::PoolStats;
use crate::pool::{Job, Pool, PoolConfig};
use crate::runtime::{self, lock};

/// Default for [`Builder::max_blocking_threads`](crate::Builder::max_blocking_threads).
pub(crate) const DEFAULT_MAX_BLOCKING: usize = 128;
/// How long an idle lane thread waits for work before it ends.
const IDLE: Duration = Duration::from_secs(10);
/// The nice value of compute threads (workers run at 0).
const COMPUTE_NICE: i32 = 10;
/// Default backlog per compute thread for [`try_spawn_compute`].
pub(crate) const BACKLOG_PER_THREAD: usize = 4;

/// The lane settings a runtime starts with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct LaneConfig {
    pub(crate) max_blocking: usize,
    pub(crate) compute_threads: usize,
    pub(crate) compute_backlog: usize,
}

/// A runtime's two lanes. No thread starts until the first job.
pub(crate) struct Lanes {
    blocking: Arc<Pool>,
    compute: Arc<Pool>,
    compute_backlog: usize,
}

impl Lanes {
    pub(crate) fn new(cfg: LaneConfig) -> Self {
        Self {
            blocking: Pool::new(PoolConfig {
                name: "giong-lane-block",
                max_threads: cfg.max_blocking,
                idle: IDLE,
                nice: 0,
                stack_size: None,
                run_inline_on_failure: false,
                fair: false,
            }),
            compute: Pool::new(PoolConfig {
                name: "giong-lane-cpu",
                max_threads: cfg.compute_threads,
                idle: IDLE,
                nice: COMPUTE_NICE,
                stack_size: None,
                run_inline_on_failure: false,
                fair: true,
            }),
            compute_backlog: cfg.compute_backlog,
        }
    }

    /// No closure that has not started will run; their handles resolve to
    /// [`JoinError::Cancelled`].
    pub(crate) fn stop(&self) {
        // Dropped outside the pools' locks: each dropped job completes its
        // handle, which wakes whoever awaits it.
        drop(self.blocking.stop());
        drop(self.compute.stop());
    }

    pub(crate) fn stats(&self) -> LaneStats {
        LaneStats {
            blocking: self.blocking.stats(),
            compute: self.compute.stats(),
        }
    }

    pub(crate) fn spawn_blocking<F, T>(&self, f: F) -> BlockingHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (handle, job) = job(f);
        // A stopped pool gives the job back; dropping it cancels the handle.
        drop(self.blocking.spawn(job));
        handle
    }

    pub(crate) fn spawn_compute<F, T>(&self, f: F) -> BlockingHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // Inside a budgeted task: in the budget's queue, charged to it.
        if let Some(charge) = crate::budget::charge_compute() {
            return self.spawn_compute_in(charge, f);
        }
        let (handle, job) = job(f);
        drop(self.compute.spawn(job));
        handle
    }

    /// Queues `f` in the compute queue of the budget `charge` belongs to.
    /// The charge is given back when `f` returns, or when it is dropped
    /// without running.
    pub(crate) fn spawn_compute_in<F, T>(&self, charge: ComputeCharge, f: F) -> BlockingHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let (class, weight) = (charge.class(), charge.weight());
        let (handle, job) = job(move || {
            let value = f();
            drop(charge);
            value
        });
        drop(self.compute.spawn_in(class, weight, job));
        handle
    }

    pub(crate) fn try_spawn_compute<F, T>(&self, f: F) -> io::Result<BlockingHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // Inside a budgeted task: the budget's limit decides.
        if let Some(budget) = crate::budget::current() {
            return budget.try_spawn_compute(f);
        }
        let (handle, job) = job(f);
        let backlog = self.compute_backlog;
        let mut refused = false;
        let result = self.compute.spawn_if(job, |queued| {
            refused = queued >= backlog;
            !refused
        });
        if refused {
            // Do not report the refusal as a cancellation to anyone: the
            // handle is dropped with the job, and the caller gets the error.
            drop(result);
            return Err(raise(
                Code::ComputeBusy,
                format!("the compute lane already has {backlog} jobs waiting for a thread"),
                "shed the request (answer 503 or retry later), or raise \
                 Builder::compute_backlog or Builder::compute_threads if the \
                 machine has CPU to spare",
            ));
        }
        drop(result);
        Ok(handle)
    }
}

/// What the two lanes of a runtime are doing, from
/// [`Handle::lane_stats`](crate::Handle::lane_stats).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct LaneStats {
    /// The blocking lane ([`spawn_blocking`]).
    pub blocking: PoolStats,
    /// The compute lane ([`spawn_compute`]).
    pub compute: PoolStats,
}

// ------------------------------------------------------------------ handle

/// Queued, not started.
const QUEUED: u8 = 0;
/// The closure is running or has run.
const STARTED: u8 = 1;
/// Aborted before it started.
const ABORTED: u8 = 2;

struct Slot<T> {
    phase: AtomicU8,
    state: Mutex<SlotState<T>>,
}

struct SlotState<T> {
    result: Option<Result<T, JoinError>>,
    waker: Option<Waker>,
    finished: bool,
    /// The handle took the result.
    taken: bool,
}

impl<T> Slot<T> {
    fn complete(&self, result: Result<T, JoinError>) {
        let waker = {
            let mut state = lock(&self.state);
            if state.finished {
                return;
            }
            state.finished = true;
            state.result = Some(result);
            state.waker.take()
        };
        // Woken outside the lock: giong's wakers are thread-safe, so this
        // reaches the awaiting task on whichever worker it lives.
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Completes the slot as cancelled if the job is dropped without running:
/// the runtime stopped before a thread took it.
struct Pending<T>(Arc<Slot<T>>);

impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        self.0.complete(Err(JoinError::Cancelled));
    }
}

fn job<F, T>(f: F) -> (BlockingHandle<T>, Job)
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let slot = Arc::new(Slot {
        phase: AtomicU8::new(QUEUED),
        state: Mutex::new(SlotState {
            result: None,
            waker: None,
            finished: false,
            taken: false,
        }),
    });
    let pending = Pending(slot.clone());
    let job: Job = Box::new(move || {
        let slot = &pending.0;
        if slot
            .phase
            .compare_exchange(QUEUED, STARTED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Aborted: the handle already resolved.
            return;
        }
        let result = panic::catch_unwind(AssertUnwindSafe(f)).map_err(JoinError::Panic);
        slot.complete(result);
        // `pending` drops here and finds the slot finished.
    });
    (BlockingHandle { slot }, job)
}

/// A handle to a closure on the blocking or the compute lane. Awaiting it
/// gives the closure's value.
///
/// It resolves to [`JoinError::Panic`] if the closure panicked (the panic
/// does not reach the worker), and to [`JoinError::Cancelled`] if the
/// closure never ran: [`abort`](Self::abort) was called before it started,
/// or the runtime stopped first.
///
/// Dropping the handle lets the closure run on its own. Unlike a
/// [`JoinHandle`](crate::JoinHandle), it is `Send` when `T` is, and may be
/// awaited on any worker or thread.
#[must_use = "a lane closure runs whether or not its handle is awaited; await it for the result"]
pub struct BlockingHandle<T> {
    slot: Arc<Slot<T>>,
}

impl<T> BlockingHandle<T> {
    /// Cancels the closure if no thread has started it: it will not run, and
    /// the handle resolves to [`JoinError::Cancelled`]. Returns `true` if it
    /// was cancelled, `false` if it had already started or finished (a
    /// closure that runs cannot be stopped from outside; it runs to its end
    /// and the handle gives its value).
    pub fn abort(&self) -> bool {
        let aborted = self
            .slot
            .phase
            .compare_exchange(QUEUED, ABORTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        if aborted {
            self.slot.complete(Err(JoinError::Cancelled));
        }
        aborted
    }

    /// `true` once the closure has returned or panicked, or will never run.
    pub fn is_finished(&self) -> bool {
        lock(&self.slot.state).finished
    }
}

impl<T> Future for BlockingHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = lock(&self.slot.state);
        if let Some(result) = state.result.take() {
            state.taken = true;
            return Poll::Ready(result);
        }
        assert!(!state.taken, "BlockingHandle polled after completion");
        match &state.waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => state.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl<T> fmt::Debug for BlockingHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BlockingHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

// ------------------------------------------------------------- entry points

fn lanes<R>(f: impl FnOnce(&Lanes) -> R) -> R {
    runtime::with_worker(|w| f(&w.rt.lanes))
}

/// Runs a closure that **blocks** (waits on a system call, a lock, a
/// synchronous library) on the runtime's blocking lane, and returns a handle
/// to its value.
///
/// Use it for `std::fs` and `std::net` code, C libraries, `getaddrinfo`, or
/// any call that may sleep. For work that keeps a CPU busy use
/// [`spawn_compute`]; for giong's own I/O no lane is needed.
///
/// The lane starts a thread whenever every thread is busy, up to
/// [`Builder::max_blocking_threads`](crate::Builder::max_blocking_threads);
/// beyond that, closures wait in order. See the [module docs](crate::lane).
///
/// ```no_run
/// giong::block_on(async {
///     let text = giong::spawn_blocking(|| std::fs::read_to_string("/etc/hostname"))
///         .await
///         .expect("the closure panicked")?;
///     println!("{text}");
///     Ok::<(), std::io::Error>(())
/// })?;
/// # Ok::<(), std::io::Error>(())
/// ```
///
/// # Panics
///
/// Panics with `GIONG-E0011` outside a giong runtime. From a plain thread,
/// use [`Handle::spawn_blocking`](crate::Handle::spawn_blocking).
#[track_caller]
pub fn spawn_blocking<F, T>(f: F) -> BlockingHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    lanes(|l| l.spawn_blocking(f))
}

/// Runs a **CPU-bound** closure on the runtime's compute lane, and returns a
/// handle to its value.
///
/// Use it for anything that computes for longer than about 100 µs without
/// waiting: hashing a password, resizing an image, compressing, parsing or
/// serialising a large document, inference. The lane has at most
/// [`Builder::compute_threads`](crate::Builder::compute_threads) threads (by
/// default one per CPU the process may use), running at a lower priority
/// than the workers so that network work keeps its latency. Closures beyond
/// that wait in order, without limit; [`try_spawn_compute`] refuses instead
/// when too many wait. See the [module docs](crate::lane).
///
/// To spread one big job over several threads, split it into closures, or
/// run a data-parallel library such as Rayon inside one closure.
///
/// ```no_run
/// # fn checksum(_: &[u8]) -> u64 { 0 }
/// giong::block_on(async {
///     let data = vec![0u8; 64 << 20];
///     let sum = giong::spawn_compute(move || checksum(&data)).await.unwrap();
///     println!("{sum:x}");
/// });
/// ```
///
/// # Panics
///
/// Panics with `GIONG-E0011` outside a giong runtime. From a plain thread,
/// use [`Handle::spawn_compute`](crate::Handle::spawn_compute).
#[track_caller]
pub fn spawn_compute<F, T>(f: F) -> BlockingHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    lanes(|l| l.spawn_compute(f))
}

/// Like [`spawn_compute`], but refuses the closure when the compute lane is
/// already behind.
///
/// # Errors
///
/// [`GIONG-E0014`](crate::error::Code::ComputeBusy) (`WouldBlock`) when
/// [`Builder::compute_backlog`](crate::Builder::compute_backlog) closures
/// already wait for a thread. The closure is dropped without running.
///
/// ```no_run
/// # fn thumbnail(_: &[u8]) -> Vec<u8> { Vec::new() }
/// use giong::error::{self, Code};
///
/// # async fn handle(image: Vec<u8>) -> std::io::Result<(u16, Vec<u8>)> {
/// match giong::try_spawn_compute(move || thumbnail(&image)) {
///     Ok(job) => Ok((200, job.await.expect("thumbnail panicked"))),
///     Err(e) if error::code(&e) == Some(Code::ComputeBusy) => Ok((503, Vec::new())),
///     Err(e) => Err(e),
/// }
/// # }
/// ```
///
/// # Panics
///
/// Panics with `GIONG-E0011` outside a giong runtime.
#[track_caller]
pub fn try_spawn_compute<F, T>(f: F) -> io::Result<BlockingHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    lanes(|l| l.try_spawn_compute(f))
}
