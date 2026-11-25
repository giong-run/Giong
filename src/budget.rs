//! Budgets: a share of the runtime for one tenant, so that one cannot take
//! what the others need.
//!
//! A server shared by several tenants (customers, API keys, classes of
//! traffic) serves all of them from the same workers, memory and compute
//! threads. Without limits, the tenant that sends the most work gets the
//! most of everything: its tasks fill the run queues, its buffers fill the
//! memory, its jobs fill the compute lane, and every other tenant waits
//! behind it or misses its deadlines. A [`Budget`] gives each tenant limits
//! of its own and charges each resource to the tenant that holds it:
//!
//! | Resource | Charged | Released | Limit |
//! |---|---|---|---|
//! | Tasks | [`Budget::spawn`], and every [`spawn`](crate::spawn) made inside a task of the budget | When the task ends or is aborted | [`Limits::tasks`] |
//! | Memory | [`Budget::reserve`], and the size of each task's future | When the [`Reservation`] or the task is dropped | [`Limits::memory`] |
//! | I/O in flight | Each I/O operation a task of the budget starts (a send, a write, a connect, a file read) | When the **kernel** is done with it, not when its future is dropped | [`Limits::io`] |
//! | Compute | [`Budget::spawn_compute`], and every [`spawn_compute`](crate::spawn_compute) made inside a task of the budget | When the closure returns, or is dropped without running | [`Limits::compute`] |
//!
//! The compute lane also serves budgets fairly: each budget has its own
//! queue there, and a free compute thread takes the next closure of the
//! budget that has used the least compute time for its
//! [`compute_weight`](Limits::compute_weight). A tenant whose jobs take
//! 50 ms each cannot make another tenant's 1 ms jobs wait behind them.
//!
//! ```no_run
//! use giong::budget::{Budget, Limits};
//!
//! # fn render(_: &[u8]) -> Vec<u8> { Vec::new() }
//! giong::block_on(async {
//!     let limits = Limits::new().tasks(1_000).memory(32 << 20).compute(8);
//!     let tenant = Budget::new("tenant-a", limits);
//!
//!     // Waits for a task slot; the task, and everything it spawns, is
//!     // charged to the tenant.
//!     let task = tenant
//!         .spawn(async {
//!             // Waits until the tenant has 1 MiB to spare.
//!             let _memory = giong::budget::current()
//!                 .expect("inside the tenant's task")
//!                 .reserve(1 << 20)
//!                 .await?;
//!             // Queued in the tenant's own queue on the compute lane.
//!             let page = giong::spawn_compute(|| render(b"...")).await.expect("render panicked");
//!             Ok::<_, std::io::Error>(page)
//!         })
//!         .await;
//!     let _page = task.await.expect("the task panicked")?;
//!     Ok::<(), std::io::Error>(())
//! })?;
//! # Ok::<(), std::io::Error>(())
//! ```
//!
//! ## Waiting or refusing
//!
//! As everywhere in giong, a limit **waits** by default: [`Budget::spawn`],
//! [`Budget::reserve`] and [`Budget::spawn_compute`] return once the budget
//! has room, in the order they asked. Under overload, waiting work piles up;
//! a server that would rather answer `503` at once uses the `try_` forms,
//! [`Budget::try_spawn`], [`Budget::try_reserve`] and
//! [`Budget::try_spawn_compute`], which refuse with
//! [`GIONG-E0080`](crate::error::Code::BudgetFull) instead. Wrap a waiting
//! call in [`time::timeout`](crate::time::timeout) to bound the wait.
//!
//! Spawns and compute closures made inside a budgeted task with the plain
//! [`spawn`](crate::spawn) and [`spawn_compute`](crate::spawn_compute) are
//! charged without waiting, since those functions cannot wait or fail: the
//! budget's use may then pass its limit for a while, and its waiting and
//! `try_` calls wait or refuse until it is back under.
//!
//! An I/O operation started inside a budgeted task waits for an I/O slot
//! when the budget has [`Limits::io`] operations in flight. Size that limit
//! well above the operations the tenant's tasks keep pending at once: a
//! task whose send waits for a slot while the tenant's other operations
//! wait for that send (a proxy reading the answer to a request it has not
//! sent yet) waits forever. The default is no limit.
//!
//! ## What is not charged
//!
//! * Memory the tenant's code allocates without a [`Reservation`]: the
//!   budget counts what is reserved, plus each task's future. Reserve what
//!   a request will hold (its body, its buffers) when it starts.
//! * Multishot operations (a listener's accept, a TCP stream's receive),
//!   which belong to a connection, not to the work it does, and operations
//!   the driver finishes at once without waiting.
//! * Work spawned on another worker ([`Handle::spawn_on`](crate::Handle::spawn_on)),
//!   and the blocking lane.
//! * Time on the workers. A budget bounds how much a tenant can have at
//!   once, which bounds its share of a worker's run queue; a task that
//!   computes for long on a worker holds it up for every tenant, so heavy
//!   work belongs on the compute lane, where it is shared fairly.
//!
//! ## Cost
//!
//! Next to nothing for code that uses no budget: the start of an operation
//! and a spawn (once the task is allocated) test one pointer the worker keeps
//! (whether the task polled now runs in a budget), and everything else is out
//! of line. Measured with `scripts/instr.py`: 0 to 0.3% more instructions per
//! operation, and a spawn takes fewer than before this module.
//! Inside a budget, each charge is one atomic add on the budget's counter;
//! a budget is shared by every worker its tenant runs on.

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use crate::error::{raise, Code};
use crate::executor::JoinHandle;
use crate::lane::BlockingHandle;
use crate::runtime::{self, lock, Worker};

/// No limit.
const UNLIMITED: usize = usize::MAX;

/// What a task costs beyond its future, roughly: the task's header, its
/// waker and its join state.
const TASK_OVERHEAD: usize = 128;

// -------------------------------------------------------------------- limits

/// The limits of a [`Budget`]. Every limit starts unset (no limit).
///
/// ```
/// use giong::budget::Limits;
///
/// let limits = Limits::new()
///     .tasks(10_000)          // tasks alive at once
///     .memory(64 << 20)       // bytes reserved, task futures included
///     .io(4_096)              // I/O operations in flight
///     .compute(16)            // compute closures queued or running
///     .compute_weight(2);     // twice the compute share of weight 1
/// # let _ = limits;
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    tasks: usize,
    memory: usize,
    io: usize,
    compute: usize,
    weight: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new()
    }
}

impl Limits {
    /// No limit on anything, and compute weight 1.
    pub const fn new() -> Self {
        Self {
            tasks: UNLIMITED,
            memory: UNLIMITED,
            io: UNLIMITED,
            compute: UNLIMITED,
            weight: 1,
        }
    }

    /// The most tasks of the budget alive at once (at least 1).
    #[must_use]
    pub const fn tasks(mut self, max: usize) -> Self {
        self.tasks = if max == 0 { 1 } else { max };
        self
    }

    /// The most bytes reserved at once ([`Budget::reserve`]), the futures of
    /// the budget's tasks included.
    #[must_use]
    pub const fn memory(mut self, max_bytes: usize) -> Self {
        self.memory = max_bytes;
        self
    }

    /// The most I/O operations of the budget's tasks in flight at once (at
    /// least 1). See the [module docs](self#waiting-or-refusing) for how to
    /// size it.
    #[must_use]
    pub const fn io(mut self, max: usize) -> Self {
        self.io = if max == 0 { 1 } else { max };
        self
    }

    /// The most closures of the budget queued or running on the compute lane
    /// at once (at least 1).
    #[must_use]
    pub const fn compute(mut self, max: usize) -> Self {
        self.compute = if max == 0 { 1 } else { max };
        self
    }

    /// The budget's share of the compute lane when it is busy, relative to
    /// other budgets (at least 1, default 1). A budget of weight 2 gets
    /// twice the compute time of a budget of weight 1 while both have work
    /// queued; work outside any budget has weight 1.
    #[must_use]
    pub const fn compute_weight(mut self, weight: u32) -> Self {
        self.weight = if weight == 0 { 1 } else { weight };
        self
    }
}

// ---------------------------------------------------------------------- gate

/// A count with a limit, taken and given back from any thread. Taking is
/// one atomic operation while there is room and nobody waits; waiters queue
/// in order and are handed what is given back.
struct Gate {
    used: AtomicUsize,
    limit: AtomicUsize,
    peak: AtomicUsize,
    /// Waiters in `queue`, readable without its lock.
    waiting: AtomicUsize,
    queue: Mutex<VecDeque<Arc<Waiter>>>,
    refused: AtomicU64,
    waited: AtomicU64,
}

pub(crate) struct Waiter {
    need: usize,
    /// The units are taken for this waiter; it owns them.
    granted: AtomicBool,
    /// The limit was lowered below `need`: it can never be granted, and
    /// left the queue.
    refused: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            used: AtomicUsize::new(0),
            limit: AtomicUsize::new(limit),
            peak: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
            queue: Mutex::new(VecDeque::new()),
            refused: AtomicU64::new(0),
            waited: AtomicU64::new(0),
        }
    }

    fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }

    /// Takes `n` if they fit under the limit.
    fn take(&self, n: usize) -> bool {
        let limit = self.limit.load(Ordering::Relaxed);
        let mut used = self.used.load(Ordering::SeqCst);
        loop {
            match used.checked_add(n) {
                Some(next) if next <= limit => {
                    match self.used.compare_exchange_weak(
                        used,
                        next,
                        Ordering::SeqCst,
                        Ordering::SeqCst,
                    ) {
                        Ok(_) => {
                            self.peak.fetch_max(next, Ordering::Relaxed);
                            return true;
                        }
                        Err(now) => used = now,
                    }
                }
                _ => return false,
            }
        }
    }

    /// Takes `n` if they fit and nobody waits before.
    fn try_take(&self, n: usize) -> bool {
        self.waiting.load(Ordering::SeqCst) == 0 && self.take(n)
    }

    /// Takes `n` whether or not they fit.
    fn force(&self, n: usize) {
        let next = self.used.fetch_add(n, Ordering::SeqCst) + n;
        self.peak.fetch_max(next, Ordering::Relaxed);
    }

    fn give(&self, n: usize) {
        if n == 0 {
            return;
        }
        self.used.fetch_sub(n, Ordering::SeqCst);
        if self.waiting.load(Ordering::SeqCst) > 0 {
            self.grant();
        }
    }

    /// Hands what fits to the waiters at the front of the queue, in order.
    fn grant(&self) {
        let mut woken = Vec::new();
        {
            let mut queue = lock(&self.queue);
            self.grant_locked(&mut queue, &mut woken);
        }
        for waker in woken {
            waker.wake();
        }
    }

    /// A waiter that needs more than the whole limit (lowered since it
    /// queued) is refused rather than left at the front, where it would
    /// hold up every waiter behind it for ever.
    fn grant_locked(&self, queue: &mut VecDeque<Arc<Waiter>>, woken: &mut Vec<Waker>) {
        while let Some(front) = queue.front() {
            let fits = front.need <= self.limit.load(Ordering::SeqCst);
            if fits && !self.take(front.need) {
                break;
            }
            let front = queue.pop_front().expect("front exists");
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            if fits {
                front.granted.store(true, Ordering::Release);
            } else {
                front.refused.store(true, Ordering::Release);
            }
            let waker = lock(&front.waker).take();
            if let Some(waker) = waker {
                woken.push(waker);
            }
        }
    }

    /// Takes `n` now, or queues a waiter that will be handed them.
    fn take_or_wait(&self, n: usize, waker: &Waker) -> Option<Arc<Waiter>> {
        if self.try_take(n) {
            return None;
        }
        self.waited.fetch_add(1, Ordering::Relaxed);
        let waiter = Arc::new(Waiter {
            need: n,
            granted: AtomicBool::new(false),
            refused: AtomicBool::new(false),
            waker: Mutex::new(Some(waker.clone())),
        });
        let mut woken = Vec::new();
        {
            let mut queue = lock(&self.queue);
            // Counted before looking again, so that a `give` racing with
            // this either sees the waiter or left room that `grant` finds.
            self.waiting.fetch_add(1, Ordering::SeqCst);
            queue.push_back(waiter.clone());
            self.grant_locked(&mut queue, &mut woken);
        }
        for w in woken {
            w.wake();
        }
        Some(waiter)
    }

    /// `Some(true)` once `waiter` was handed its units, `Some(false)` once
    /// it was refused; otherwise keeps `waker`.
    fn poll_waiter(&self, waiter: &Waiter, waker: &Waker) -> Option<bool> {
        let done = || {
            if waiter.granted.load(Ordering::Acquire) {
                Some(true)
            } else if waiter.refused.load(Ordering::Acquire) {
                Some(false)
            } else {
                None
            }
        };
        if let Some(done) = done() {
            return Some(done);
        }
        let mut slot = lock(&waiter.waker);
        // Decided between the two looks: the granter took the old waker.
        if let Some(done) = done() {
            return Some(done);
        }
        match &*slot {
            Some(w) if w.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
        None
    }

    /// The waiting future is gone: leave the queue, or give back what it
    /// was handed.
    fn cancel(&self, waiter: &Arc<Waiter>) {
        let mut woken = Vec::new();
        let granted = {
            let mut queue = lock(&self.queue);
            if waiter.granted.load(Ordering::Acquire) {
                true
            } else {
                if let Some(at) = queue.iter().position(|w| Arc::ptr_eq(w, waiter)) {
                    queue.remove(at);
                    self.waiting.fetch_sub(1, Ordering::SeqCst);
                }
                // It may have held up smaller waiters behind it.
                self.grant_locked(&mut queue, &mut woken);
                false
            }
        };
        for w in woken {
            w.wake();
        }
        if granted {
            self.give(waiter.need);
        }
    }

    fn set_limit(&self, limit: usize) {
        self.limit.store(limit, Ordering::SeqCst);
        if self.waiting.load(Ordering::SeqCst) > 0 {
            self.grant();
        }
    }

    fn stats(&self) -> GateStats {
        let limit = self.limit();
        GateStats {
            used: self.used.load(Ordering::Relaxed),
            limit: (limit != UNLIMITED).then_some(limit),
            peak: self.peak.load(Ordering::Relaxed),
            waiting: self.waiting.load(Ordering::Relaxed),
            waited: self.waited.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
        }
    }
}

/// One resource of a budget, from [`Budget::stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct GateStats {
    /// In use now. May pass `limit` after charges that do not wait (see the
    /// [module docs](self#waiting-or-refusing)).
    pub used: usize,
    /// The limit, if there is one.
    pub limit: Option<usize>,
    /// The most in use at once since the budget was created.
    pub peak: usize,
    /// Calls waiting for room now.
    pub waiting: usize,
    /// Calls that had to wait, since the budget was created.
    pub waited: u64,
    /// `try_` calls refused, since the budget was created.
    pub refused: u64,
}

/// What a budget holds right now, from [`Budget::stats`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BudgetStats {
    /// The budget's name.
    pub name: String,
    /// Tasks alive.
    pub tasks: GateStats,
    /// Bytes reserved, task futures included.
    pub memory: GateStats,
    /// I/O operations in flight.
    pub io: GateStats,
    /// Compute closures queued or running.
    pub compute: GateStats,
    /// The compute weight.
    pub compute_weight: u32,
}

// -------------------------------------------------------------------- budget

pub(crate) struct Inner {
    name: Box<str>,
    /// The budget's queue on the compute lane.
    class: u64,
    weight: AtomicUsize,
    tasks: Gate,
    memory: Gate,
    io: Gate,
    compute: Gate,
    /// Operations waiting for an I/O slot; all are woken when one is given
    /// back, and try again.
    io_parked: Mutex<Vec<Waker>>,
    io_parked_len: AtomicUsize,
}

/// A share of the runtime for one tenant: limits on its tasks, memory, I/O
/// in flight and compute closures, each charged to the budget while the
/// tenant holds it. See the [module docs](self).
///
/// Cloning gives another handle to the same budget. It is `Send` and `Sync`:
/// one budget serves its tenant on every worker.
#[derive(Clone)]
pub struct Budget {
    inner: Arc<Inner>,
}

impl fmt::Debug for Budget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Budget")
            .field("name", &self.inner.name)
            .field("tasks", &self.inner.tasks.used.load(Ordering::Relaxed))
            .field("memory", &self.inner.memory.used.load(Ordering::Relaxed))
            .field("io", &self.inner.io.used.load(Ordering::Relaxed))
            .field("compute", &self.inner.compute.used.load(Ordering::Relaxed))
            .finish()
    }
}

/// Classes of the compute lane; 0 is work outside any budget.
static NEXT_CLASS: AtomicU64 = AtomicU64::new(1);

impl Budget {
    /// A budget named `name` (for [`stats`](Self::stats), errors and logs)
    /// with `limits`.
    pub fn new(name: impl Into<String>, limits: Limits) -> Self {
        Self {
            inner: Arc::new(Inner {
                name: name.into().into_boxed_str(),
                class: NEXT_CLASS.fetch_add(1, Ordering::Relaxed),
                weight: AtomicUsize::new(limits.weight as usize),
                tasks: Gate::new(limits.tasks),
                memory: Gate::new(limits.memory),
                io: Gate::new(limits.io),
                compute: Gate::new(limits.compute),
                io_parked: Mutex::new(Vec::new()),
                io_parked_len: AtomicUsize::new(0),
            }),
        }
    }

    /// The name it was created with.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The limits now.
    pub fn limits(&self) -> Limits {
        let i = &self.inner;
        Limits {
            tasks: i.tasks.limit(),
            memory: i.memory.limit(),
            io: i.io.limit(),
            compute: i.compute.limit(),
            weight: i.weight.load(Ordering::Relaxed) as u32,
        }
    }

    /// Changes the limits from now on, for a tenant whose plan changed or a
    /// container that was resized. A lower limit takes nothing back: what
    /// is in use stays, and new work waits until use is under it; a
    /// [`reserve`](Self::reserve) waiting for more than the new memory limit
    /// fails with [`GIONG-E0081`](crate::error::Code::BudgetTooSmall), as it
    /// would have when asked. A higher one lets waiting calls through at
    /// once.
    pub fn set_limits(&self, limits: Limits) {
        let i = &self.inner;
        i.weight.store(limits.weight as usize, Ordering::Relaxed);
        i.tasks.set_limit(limits.tasks);
        i.memory.set_limit(limits.memory);
        i.io.set_limit(limits.io);
        // Operations waiting for an I/O slot try again under the new limit.
        if i.io_parked_len.load(Ordering::SeqCst) > 0 {
            i.io.used.fetch_add(1, Ordering::SeqCst);
            give_io(Arc::as_ptr(&self.inner));
        }
        i.compute.set_limit(limits.compute);
    }

    /// What the budget holds now, and its counts of waits and refusals.
    pub fn stats(&self) -> BudgetStats {
        let i = &self.inner;
        BudgetStats {
            name: i.name.to_string(),
            tasks: i.tasks.stats(),
            memory: i.memory.stats(),
            io: GateStats {
                waiting: i.io_parked_len.load(Ordering::Relaxed),
                ..i.io.stats()
            },
            compute: i.compute.stats(),
            compute_weight: i.weight.load(Ordering::Relaxed) as u32,
        }
    }

    /// Spawns `fut` as a task of the budget on this worker, once the budget
    /// has a task slot free; the task and everything it spawns are charged
    /// to the budget (see the [module docs](self)).
    ///
    /// Cancel safe: dropped while it waits, it spawns nothing and takes no
    /// slot.
    ///
    /// # Panics
    ///
    /// Panics with `GIONG-E0011` outside a giong runtime.
    #[track_caller]
    pub fn spawn<F>(&self, fut: F) -> impl Future<Output = JoinHandle<F::Output>> + '_
    where
        F: Future + 'static,
    {
        let location = std::panic::Location::caller();
        // The handle is the value: the task runs whether or not it is awaited.
        #[allow(clippy::async_yields_async)]
        async move {
            // Never refused: a task limit is at least 1.
            let _granted = Acquire::new(&self.inner.tasks, 1).await;
            spawn_charged(self.inner.clone(), fut, location)
        }
    }

    /// Like [`spawn`](Self::spawn), but refuses when the budget has no task
    /// slot free.
    ///
    /// # Errors
    ///
    /// [`GIONG-E0080`](crate::error::Code::BudgetFull) (`WouldBlock`) when
    /// [`Limits::tasks`] tasks of the budget are alive, or others wait for a
    /// slot. `fut` is dropped without running.
    ///
    /// # Panics
    ///
    /// Panics with `GIONG-E0011` outside a giong runtime.
    #[track_caller]
    pub fn try_spawn<F>(&self, fut: F) -> io::Result<JoinHandle<F::Output>>
    where
        F: Future + 'static,
    {
        let location = std::panic::Location::caller();
        if !self.inner.tasks.try_take(1) {
            return Err(self.refuse(&self.inner.tasks, "tasks", "a task"));
        }
        Ok(spawn_charged(self.inner.clone(), fut, location))
    }

    /// Reserves `bytes` of the budget's memory, once it has them free, for
    /// as long as the returned [`Reservation`] lives.
    ///
    /// Reservations wait in order: a large one is not overtaken by smaller
    /// ones that come after it.
    ///
    /// Cancel safe: dropped while it waits, it reserves nothing.
    ///
    /// # Errors
    ///
    /// [`GIONG-E0081`](crate::error::Code::BudgetTooSmall) (`InvalidInput`)
    /// at once when `bytes` is more than [`Limits::memory`]: it could never
    /// fit; and while it waits, once [`set_limits`](Self::set_limits)
    /// lowers the limit below `bytes`.
    pub async fn reserve(&self, bytes: usize) -> io::Result<Reservation> {
        // Refused: the limit was lowered below `bytes` while it waited, and
        // the check says so (unless it was raised again meanwhile).
        loop {
            self.check_fits(bytes)?;
            if Acquire::new(&self.inner.memory, bytes).await {
                break;
            }
        }
        Ok(Reservation {
            budget: self.inner.clone(),
            bytes,
        })
    }

    /// Like [`reserve`](Self::reserve), but refuses when the memory is not
    /// free now.
    ///
    /// # Errors
    ///
    /// [`GIONG-E0080`](crate::error::Code::BudgetFull) (`WouldBlock`) when
    /// `bytes` do not fit under [`Limits::memory`] now, or other
    /// reservations wait; [`GIONG-E0081`](crate::error::Code::BudgetTooSmall)
    /// when they never could.
    pub fn try_reserve(&self, bytes: usize) -> io::Result<Reservation> {
        self.check_fits(bytes)?;
        if !self.inner.memory.try_take(bytes) {
            return Err(self.refuse(&self.inner.memory, "bytes of memory", "the reservation"));
        }
        Ok(Reservation {
            budget: self.inner.clone(),
            bytes,
        })
    }

    /// Runs a CPU-bound closure on the compute lane, in the budget's own
    /// queue there, once the budget has a compute slot free. See
    /// [`spawn_compute`](crate::spawn_compute) for the lane and the
    /// [module docs](self) for how budgets share it.
    ///
    /// Cancel safe: dropped while it waits, it queues nothing.
    ///
    /// # Panics
    ///
    /// Panics with `GIONG-E0011` outside a giong runtime.
    pub async fn spawn_compute<F, T>(&self, f: F) -> BlockingHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        // Never refused: a compute limit is at least 1.
        let _granted = Acquire::new(&self.inner.compute, 1).await;
        let charge = ComputeCharge(self.inner.clone());
        runtime::with_worker(|w| w.rt.lanes.spawn_compute_in(charge, f))
    }

    /// Like [`spawn_compute`](Self::spawn_compute), but refuses when the
    /// budget has no compute slot free.
    ///
    /// # Errors
    ///
    /// [`GIONG-E0080`](crate::error::Code::BudgetFull) (`WouldBlock`) when
    /// [`Limits::compute`] closures of the budget are queued or running, or
    /// others wait for a slot. The closure is dropped without running.
    ///
    /// # Panics
    ///
    /// Panics with `GIONG-E0011` outside a giong runtime.
    pub fn try_spawn_compute<F, T>(&self, f: F) -> io::Result<BlockingHandle<T>>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        if !self.inner.compute.try_take(1) {
            return Err(self.refuse(&self.inner.compute, "compute closures", "the closure"));
        }
        let charge = ComputeCharge(self.inner.clone());
        Ok(runtime::with_worker(|w| {
            w.rt.lanes.spawn_compute_in(charge, f)
        }))
    }

    /// Runs `fut` in the budget without a task of its own: the I/O it
    /// starts, the tasks it spawns and the compute closures it queues are
    /// charged to the budget. For a request served on its connection's task,
    /// where a new task per request would cost more than it gives.
    pub fn enter<F: Future>(&self, fut: F) -> InBudget<F> {
        InBudget {
            budget: self.inner.clone(),
            fut,
        }
    }

    fn check_fits(&self, bytes: usize) -> io::Result<()> {
        let limit = self.inner.memory.limit();
        if bytes > limit {
            return Err(raise(
                Code::BudgetTooSmall,
                format!(
                    "a reservation of {bytes} bytes asked budget \"{}\" for more than its \
                     whole memory limit of {limit} bytes, so it could never be granted",
                    self.inner.name
                ),
                "raise the budget's memory limit (Limits::memory) above the largest \
                 reservation, or reserve less at once (in parts, as the data arrives)",
            ));
        }
        Ok(())
    }

    #[cold]
    fn refuse(&self, gate: &Gate, what: &str, dropped: &str) -> io::Error {
        gate.refused.fetch_add(1, Ordering::Relaxed);
        let s = gate.stats();
        raise(
            Code::BudgetFull,
            format!(
                "budget \"{}\" is at its limit of {} {what} ({} in use, {} waiting), so {dropped} \
                 was refused",
                self.inner.name,
                s.limit.unwrap_or(usize::MAX),
                s.used,
                s.waiting
            ),
            "shed the work (answer 503 or retry later): the tenant has as much in progress as \
             its limits allow. If the limit is too low for the tenant's normal load, raise it \
             with Budget::set_limits; to wait instead, use the waiting form of the call",
        )
    }
}

/// The budget of the task running now, if it runs in one.
///
/// # Panics
///
/// Never; outside a runtime it is `None`.
pub fn current() -> Option<Budget> {
    let ptr = runtime::try_with_worker(|w| w.budget.get()).unwrap_or(ptr::null());
    // SAFETY: read from the worker on this thread (see `arc_of`); the new
    // handle takes a strong count of its own.
    unsafe { arc_of(ptr) }.map(|arc| Budget {
        inner: Arc::clone(&arc),
    })
}

/// The `Arc` behind a worker's current budget pointer, without touching its
/// count; `None` for null.
///
/// # Safety
///
/// `ptr` is null or was read from the worker's `budget` field on this
/// thread, during the poll that set it: `InBudget::poll` and
/// `TaskCharge::run` set it, for the duration of the inner poll, to the
/// `Inner` of an `Arc` they hold, and put the previous value back after.
#[inline]
unsafe fn arc_of(ptr: *const Inner) -> Option<std::mem::ManuallyDrop<Arc<Inner>>> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: by the caller, the `Arc` is alive; `ManuallyDrop` leaves its
    // count unchanged.
    Some(std::mem::ManuallyDrop::new(unsafe { Arc::from_raw(ptr) }))
}

// --------------------------------------------------------------- charges

/// Memory reserved from a budget with [`Budget::reserve`], given back when
/// dropped. It is `Send`: it can travel with the data it pays for.
#[must_use = "the memory is given back as soon as the reservation is dropped"]
pub struct Reservation {
    budget: Arc<Inner>,
    bytes: usize,
}

impl Reservation {
    /// The bytes it holds.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Gives back `bytes` of it now (at most what it holds), for memory the
    /// work no longer needs.
    pub fn release(&mut self, bytes: usize) {
        let bytes = bytes.min(self.bytes);
        self.bytes -= bytes;
        self.budget.memory.give(bytes);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.memory.give(self.bytes);
    }
}

impl fmt::Debug for Reservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reservation")
            .field("budget", &self.budget.name)
            .field("bytes", &self.bytes)
            .finish()
    }
}

/// A task slot and the memory of its future, held by the executor's
/// reference to a task of the budget (see `Worker::spawn_charged`).
pub(crate) struct TaskCharge {
    budget: Arc<Inner>,
    bytes: usize,
}

impl TaskCharge {
    /// Charges a task of `F` to `budget`, whose task slot is taken.
    fn new<F>(budget: Arc<Inner>) -> Self {
        Self::with_size(budget, std::mem::size_of::<F>())
    }

    fn with_size(budget: Arc<Inner>, size: usize) -> Self {
        let bytes = size + TASK_OVERHEAD;
        budget.memory.force(bytes);
        Self { budget, bytes }
    }

    /// Charges a task of `size` bytes, spawned without waiting inside a
    /// task of the budget at `ptr` (the worker's current one).
    pub(crate) fn inherit(ptr: *const Inner, size: usize) -> Self {
        // SAFETY: read from the worker on this thread during this poll (see
        // `arc_of`).
        let budget = Arc::clone(&unsafe { arc_of(ptr) }.expect("not null"));
        budget.tasks.force(1);
        Self::with_size(budget, size)
    }

    /// Runs `poll` (one poll of the task) in the budget.
    pub(crate) fn run<R>(&self, poll: impl FnOnce() -> R) -> R {
        let ptr = Arc::as_ptr(&self.budget);
        let previous = runtime::try_with_worker(|w| w.budget.replace(ptr));
        let _restore = previous.map(Restore);
        poll()
    }
}

impl Drop for TaskCharge {
    fn drop(&mut self) {
        self.budget.tasks.give(1);
        self.budget.memory.give(self.bytes);
    }
}

/// A compute slot; given back when the closure returns or is dropped.
pub(crate) struct ComputeCharge(Arc<Inner>);

impl ComputeCharge {
    pub(crate) fn class(&self) -> u64 {
        self.0.class
    }

    pub(crate) fn weight(&self) -> u32 {
        self.0.weight.load(Ordering::Relaxed) as u32
    }

    /// The budget, for the compute lane to tell when its class is gone.
    pub(crate) fn owner(&self) -> std::sync::Weak<dyn Send + Sync> {
        let weak: std::sync::Weak<Inner> = Arc::downgrade(&self.0);
        weak
    }
}

impl Drop for ComputeCharge {
    fn drop(&mut self) {
        self.0.compute.give(1);
    }
}

/// A compute slot charged without waiting to the current budget, for
/// [`spawn_compute`](crate::spawn_compute) inside a budgeted task.
pub(crate) fn charge_compute() -> Option<ComputeCharge> {
    let ptr = runtime::try_with_worker(|w| w.budget.get())?;
    // SAFETY: read from the worker on this thread (see `arc_of`).
    let b = unsafe { arc_of(ptr) }?;
    b.compute.force(1);
    Some(ComputeCharge(Arc::clone(&b)))
}

/// An I/O slot of a budget, held for an operation by key in its worker's
/// table ([`hold_io`]) from its submission until the kernel completes it. On
/// a dropped operation it travels to the driver with the operation's
/// buffers, so it is given back with them, at the completion.
pub(crate) struct IoCharge(Arc<Inner>);

impl Drop for IoCharge {
    fn drop(&mut self) {
        give_io(Arc::as_ptr(&self.0));
    }
}

/// Takes one of the I/O slots of the budget `ptr` (read from the worker on
/// this thread during this poll), or keeps `waker` to be woken when one is
/// given back and returns `false`.
#[cold]
#[inline(never)]
pub(crate) fn take_io(ptr: *const Inner, waker: &Waker) -> bool {
    // SAFETY: read from the worker on this thread during this poll.
    let Some(b) = (unsafe { arc_of(ptr) }) else {
        return true;
    };
    if b.io.take(1) {
        return true;
    }
    b.io.waited.fetch_add(1, Ordering::Relaxed);
    {
        let mut parked = lock(&b.io_parked);
        if !parked.iter().any(|w| w.will_wake(waker)) {
            parked.push(waker.clone());
        }
        b.io_parked_len.store(parked.len(), Ordering::SeqCst);
    }
    // A slot given back between the first look and the push found no
    // waker: look again (a spare wake-up later is harmless).
    b.io.take(1)
}

/// Gives back one I/O slot of the budget `ptr` (alive: the caller holds it
/// through its operation's charge or its task) and wakes the operations
/// waiting for one, which try again.
pub(crate) fn give_io(ptr: *const Inner) {
    // SAFETY: by the caller, the budget is alive.
    let b = unsafe { &*ptr };
    b.io.used.fetch_sub(1, Ordering::SeqCst);
    if b.io_parked_len.load(Ordering::SeqCst) > 0 {
        let woken = {
            let mut parked = lock(&b.io_parked);
            b.io_parked_len.store(0, Ordering::SeqCst);
            std::mem::take(&mut *parked)
        };
        for w in woken {
            w.wake();
        }
    }
}

/// Keeps the I/O slot taken for the operation `key` on worker `w`.
#[cold]
#[inline(never)]
pub(crate) fn hold_io(w: &Worker, key: u32, ptr: *const Inner) {
    // SAFETY: read from the worker on this thread during this poll.
    let b = unsafe { arc_of(ptr) }.expect("not null");
    let mut table = w.io_charges.borrow_mut();
    let at = key as usize;
    if table.len() <= at {
        table.resize_with(at + 1, || None);
    }
    table[at] = Some(IoCharge(Arc::clone(&b)));
}

/// Takes the I/O slot kept for the operation `key` on worker `w`.
#[cold]
#[inline(never)]
pub(crate) fn release_io(w: &Worker, key: u32) -> Option<IoCharge> {
    w.io_charges
        .borrow_mut()
        .get_mut(key as usize)
        .and_then(Option::take)
}

// --------------------------------------------------------------- tasks

/// Spawns `fut` charged to `budget`: the task slot is already taken; the
/// future's memory is charged here, without waiting.
fn spawn_charged<F>(
    budget: Arc<Inner>,
    fut: F,
    location: &'static std::panic::Location<'static>,
) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let charge = TaskCharge::new::<F>(budget);
    runtime::with_worker(|w| w.spawn_charged(fut, location, charge))
}

/// A future running in a budget: what it spawns, starts and queues is
/// charged to the budget. From [`Budget::enter`].
#[must_use = "futures do nothing unless polled"]
pub struct InBudget<F> {
    budget: Arc<Inner>,
    fut: F,
}

impl<F> fmt::Debug for InBudget<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InBudget")
            .field("budget", &self.budget.name)
            .finish_non_exhaustive()
    }
}

/// Puts the previous budget back, also when the inner poll panics.
struct Restore(*const Inner);

impl Drop for Restore {
    fn drop(&mut self) {
        let previous = self.0;
        runtime::try_with_worker(|w| w.budget.set(previous));
    }
}

impl<F: Future> Future for InBudget<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: `fut` is structurally pinned: it is never moved out of
        // `self` or handed out unpinned, and `InBudget` has no `Drop` of its
        // own that could move it. The other fields are not pinned.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: as above, `fut` stays where it is for as long as `self`.
        let fut = unsafe { Pin::new_unchecked(&mut this.fut) };
        let ptr = Arc::as_ptr(&this.budget);
        // Outside a runtime (a budgeted future awaited elsewhere) nothing is
        // charged: there is no worker to start operations on.
        let previous = runtime::try_with_worker(|w| w.budget.replace(ptr));
        let _restore = previous.map(Restore);
        fut.poll(cx)
    }
}

// --------------------------------------------------------------- waiting

/// Waits until `gate` hands over `n` units: `true`, and it owns them. `false`
/// when the limit was lowered below `n` meanwhile, so they never could be.
struct Acquire<'a> {
    gate: &'a Gate,
    n: usize,
    waiter: Option<Arc<Waiter>>,
    started: bool,
}

impl<'a> Acquire<'a> {
    fn new(gate: &'a Gate, n: usize) -> Self {
        Self {
            gate,
            n,
            waiter: None,
            started: false,
        }
    }
}

impl Future for Acquire<'_> {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        let this = &mut *self;
        if !this.started {
            this.started = true;
            match this.gate.take_or_wait(this.n, cx.waker()) {
                None => return Poll::Ready(true),
                Some(waiter) => this.waiter = Some(waiter),
            }
        }
        let waiter = this.waiter.as_ref().expect("waiting");
        match this.gate.poll_waiter(waiter, cx.waker()) {
            Some(granted) => {
                this.waiter = None;
                Poll::Ready(granted)
            }
            None => Poll::Pending,
        }
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            self.gate.cancel(&waiter);
        }
    }
}
