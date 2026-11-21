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
//! Nothing for code that uses no budget: a task outside any budget reads
//! one thread-local pointer when it spawns and when it starts an operation.
//! Inside a budget, each charge is one atomic add on the budget's counter;
//! a budget is shared by every worker its tenant runs on.

use std::cell::Cell;
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
use crate::runtime::{self, lock};

/// No limit.
const UNLIMITED: usize = usize::MAX;

/// What a task costs beyond its future, roughly: the task's header, its
/// waker and its join state.
const TASK_OVERHEAD: usize = 128;

thread_local! {
    /// The budget of the task this worker polls now, or null.
    static CURRENT: Cell<*const Inner> = const { Cell::new(ptr::null()) };
}

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

    fn grant_locked(&self, queue: &mut VecDeque<Arc<Waiter>>, woken: &mut Vec<Waker>) {
        while let Some(front) = queue.front() {
            if !self.take(front.need) {
                break;
            }
            let front = queue.pop_front().expect("front exists");
            self.waiting.fetch_sub(1, Ordering::SeqCst);
            front.granted.store(true, Ordering::Release);
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
        if waiter.granted.load(Ordering::Acquire) {
            None
        } else {
            Some(waiter)
        }
    }

    /// `true` once `waiter` was handed its units; otherwise keeps `waker`.
    fn poll_waiter(&self, waiter: &Waiter, waker: &Waker) -> bool {
        if waiter.granted.load(Ordering::Acquire) {
            return true;
        }
        let mut slot = lock(&waiter.waker);
        // Granted between the two looks: the granter took the old waker.
        if waiter.granted.load(Ordering::Acquire) {
            return true;
        }
        match &*slot {
            Some(w) if w.will_wake(waker) => {}
            _ => *slot = Some(waker.clone()),
        }
        false
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
    /// is in use stays, and new work waits until use is under it. A higher
    /// one lets waiting calls through at once.
    pub fn set_limits(&self, limits: Limits) {
        let i = &self.inner;
        i.weight.store(limits.weight as usize, Ordering::Relaxed);
        i.tasks.set_limit(limits.tasks);
        i.memory.set_limit(limits.memory);
        i.io.set_limit(limits.io);
        i.compute.set_limit(limits.compute);
    }

    /// What the budget holds now, and its counts of waits and refusals.
    pub fn stats(&self) -> BudgetStats {
        let i = &self.inner;
        BudgetStats {
            name: i.name.to_string(),
            tasks: i.tasks.stats(),
            memory: i.memory.stats(),
            io: i.io.stats(),
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
            Acquire::new(&self.inner.tasks, 1).await;
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
    /// fit.
    pub async fn reserve(&self, bytes: usize) -> io::Result<Reservation> {
        self.check_fits(bytes)?;
        Acquire::new(&self.inner.memory, bytes).await;
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
        Acquire::new(&self.inner.compute, 1).await;
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
            _charge: None,
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
    let ptr = CURRENT.with(Cell::get);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: `CURRENT` is set only by `InBudget::poll`, for the duration of
    // its inner poll, to the `Inner` of an `Arc` that `InBudget` holds; this
    // runs inside that poll on the same thread, so the `Arc` is alive.
    // Taking a new strong count from it keeps the original count unchanged.
    unsafe {
        Arc::increment_strong_count(ptr);
        Some(Budget {
            inner: Arc::from_raw(ptr),
        })
    }
}

/// Runs `f` with the current budget's `Inner`, if any, without touching its
/// reference count.
#[inline]
fn with_current<R>(f: impl FnOnce(&Arc<Inner>) -> R) -> Option<R> {
    let ptr = CURRENT.with(Cell::get);
    if ptr.is_null() {
        return None;
    }
    // SAFETY: as in `current`: the `Arc` behind `ptr` is alive for the whole
    // of this call. `ManuallyDrop` keeps its count unchanged.
    let arc = std::mem::ManuallyDrop::new(unsafe { Arc::from_raw(ptr) });
    Some(f(&arc))
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

/// A task slot and the memory of its future.
struct TaskCharge {
    budget: Arc<Inner>,
    bytes: usize,
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
}

impl Drop for ComputeCharge {
    fn drop(&mut self) {
        self.0.compute.give(1);
    }
}

/// A compute slot charged without waiting to the current budget, for
/// [`spawn_compute`](crate::spawn_compute) inside a budgeted task.
pub(crate) fn charge_compute() -> Option<ComputeCharge> {
    with_current(|b| {
        b.compute.force(1);
        ComputeCharge(b.clone())
    })
}

/// An I/O slot, held by an operation from its submission until the kernel
/// completes it. On a dropped operation it travels to the driver with the
/// operation's buffers, so it is given back with them, at the completion.
pub(crate) enum IoSlot {
    Waiting(IoWait),
    // Held for its `Drop`, which gives the slot back.
    Held(#[allow(dead_code)] IoCharge),
}

pub(crate) struct IoCharge(Arc<Inner>);

impl Drop for IoCharge {
    fn drop(&mut self) {
        self.0.io.give(1);
    }
}

/// An operation waiting for an I/O slot; leaves the queue when dropped.
pub(crate) struct IoWait {
    budget: Arc<Inner>,
    waiter: Option<Arc<Waiter>>,
}

impl Drop for IoWait {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            self.budget.io.cancel(&waiter);
        }
    }
}

/// Before an operation is submitted: `Ready` once it may go (outside a
/// budget, `slot` stays `None`; inside one, it then holds the operation's
/// I/O slot), `Pending` while its budget has [`Limits::io`] operations in
/// flight.
#[inline]
pub(crate) fn poll_io(slot: &mut Option<IoSlot>, waker: &Waker) -> Poll<()> {
    match slot {
        Some(IoSlot::Held(_)) => Poll::Ready(()),
        Some(IoSlot::Waiting(wait)) => {
            let waiter = wait.waiter.as_ref().expect("an IoWait waits");
            if !wait.budget.io.poll_waiter(waiter, waker) {
                return Poll::Pending;
            }
            // Granted: the unit now belongs to the operation, so the wait
            // must not cancel when it drops.
            wait.waiter = None;
            let budget = wait.budget.clone();
            *slot = Some(IoSlot::Held(IoCharge(budget)));
            Poll::Ready(())
        }
        None => {
            let Some(next) = with_current(|b| match b.io.take_or_wait(1, waker) {
                None => IoSlot::Held(IoCharge(b.clone())),
                Some(waiter) => IoSlot::Waiting(IoWait {
                    budget: b.clone(),
                    waiter: Some(waiter),
                }),
            }) else {
                return Poll::Ready(());
            };
            let waiting = matches!(next, IoSlot::Waiting(..));
            *slot = Some(next);
            if waiting {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }
    }
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
    let bytes = std::mem::size_of::<F>() + TASK_OVERHEAD;
    budget.memory.force(bytes);
    let charge = TaskCharge {
        budget: budget.clone(),
        bytes,
    };
    let task = InBudget {
        budget,
        _charge: Some(charge),
        fut,
    };
    runtime::with_worker(|w| w.spawn_local(task, location))
}

/// [`spawn`](crate::spawn) inside a budgeted task: charged to its budget
/// without waiting. `Err` gives `fut` back outside any budget.
#[inline]
pub(crate) fn spawn_inherited<F>(
    fut: F,
    location: &'static std::panic::Location<'static>,
) -> Result<JoinHandle<F::Output>, F>
where
    F: Future + 'static,
{
    if CURRENT.with(Cell::get).is_null() {
        return Err(fut);
    }
    let budget = with_current(Arc::clone).expect("checked above");
    budget.tasks.force(1);
    Ok(spawn_charged(budget, fut, location))
}

/// A future running in a budget: what it spawns, starts and queues is
/// charged to the budget. From [`Budget::enter`]; budgeted tasks are
/// wrapped in one too.
#[must_use = "futures do nothing unless polled"]
pub struct InBudget<F> {
    budget: Arc<Inner>,
    _charge: Option<TaskCharge>,
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
        CURRENT.with(|c| c.set(self.0));
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
        let _restore = Restore(CURRENT.with(|c| c.replace(Arc::as_ptr(&this.budget))));
        fut.poll(cx)
    }
}

// --------------------------------------------------------------- waiting

/// Waits until `gate` hands over `n` units. Owns them once ready.
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
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        if !this.started {
            this.started = true;
            match this.gate.take_or_wait(this.n, cx.waker()) {
                None => return Poll::Ready(()),
                Some(waiter) => this.waiter = Some(waiter),
            }
            return Poll::Pending;
        }
        let waiter = this.waiter.as_ref().expect("waiting");
        if this.gate.poll_waiter(waiter, cx.waker()) {
            this.waiter = None;
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl Drop for Acquire<'_> {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            self.gate.cancel(&waiter);
        }
    }
}
