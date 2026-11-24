//! A pool of plain threads that grows on demand and shrinks when idle.
//!
//! The blocking and compute lanes of [`crate::lane`] use it; the drivers'
//! file pool (`driver::blocking`) has its own, which also starts threads
//! past its limit when every thread is stuck. A thread starts whenever more jobs are queued than idle
//! threads can take, up to the pool's limit, and ends after its idle time
//! without work: a job that blocks for a long time holds up only its own
//! thread.
//!
//! A pool with [`PoolConfig::fair`] (the compute lane) keeps one queue per
//! class of jobs, a [`Budget`](crate::budget::Budget) each, and gives the
//! next free thread to the class that has used the least thread time for
//! its weight (start-time fair queueing, as Linux's CFS does for threads):
//! a class with long jobs cannot make another class's short jobs wait for
//! more than about one of its jobs per thread. Other pools are plain FIFOs.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::runtime::lock;

pub(crate) type Job = Box<dyn FnOnce() + Send>;

/// How a pool behaves.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PoolConfig {
    /// The name of its threads, as `top -H` and debuggers show it.
    pub(crate) name: &'static str,
    /// The most threads at once.
    pub(crate) max_threads: usize,
    /// How long a thread waits for work before it ends.
    pub(crate) idle: Duration,
    /// The nice value its threads take on start (0 leaves it alone). On
    /// Linux the nice value belongs to each thread; on macOS, where it
    /// belongs to the process, the threads take a lower quality-of-service
    /// class instead (see `sys::lower_thread_priority`).
    pub(crate) nice: i32,
    /// Stack size of its threads; `None` for the standard library's default.
    pub(crate) stack_size: Option<usize>,
    /// One queue per class, served by thread time used for the class's
    /// weight, instead of one FIFO.
    pub(crate) fair: bool,
}

/// The class of jobs queued with [`Pool::spawn`]: no budget.
pub(crate) const NO_CLASS: u64 = 0;

/// Virtual time is thread time in nanoseconds times this, over the weight.
const WEIGHT_SCALE: u64 = 1024;

struct Class {
    jobs: VecDeque<Job>,
    /// Thread time this class has used, scaled by [`WEIGHT_SCALE`] over its
    /// weight. Only compared with other classes' virtual time.
    vtime: u64,
    weight: u64,
    /// Jobs of the class running now.
    running: usize,
}

/// The queue of a pool: one FIFO, or one per class served fairly.
#[derive(Default)]
struct Queue {
    /// The single FIFO of a pool that is not fair.
    fifo: VecDeque<Job>,
    classes: HashMap<u64, Class>,
    /// Classes with queued jobs, by virtual time.
    ready: BTreeSet<(u64, u64)>,
    /// The virtual time of the job started last: a class that had nothing
    /// queued starts from here, so time it spent idle is no credit.
    floor: u64,
    len: usize,
}

impl Queue {
    fn push(&mut self, fair: bool, class: u64, weight: u32, job: Job) {
        self.len += 1;
        if !fair {
            self.fifo.push_back(job);
            return;
        }
        let floor = self.floor;
        let c = self.classes.entry(class).or_insert_with(|| Class {
            jobs: VecDeque::new(),
            vtime: floor,
            weight: 1,
            running: 0,
        });
        c.weight = u64::from(weight.max(1));
        if c.jobs.is_empty() {
            c.vtime = c.vtime.max(floor);
            self.ready.insert((c.vtime, class));
        }
        c.jobs.push_back(job);
    }

    fn pop(&mut self, fair: bool) -> Option<(u64, Job)> {
        if !fair {
            let job = self.fifo.pop_front()?;
            self.len -= 1;
            return Some((NO_CLASS, job));
        }
        let (vtime, class) = self.ready.pop_first()?;
        let c = self.classes.get_mut(&class).expect("a ready class exists");
        let job = c.jobs.pop_front().expect("a ready class has a job");
        c.running += 1;
        if !c.jobs.is_empty() {
            self.ready.insert((vtime, class));
        }
        self.floor = self.floor.max(vtime);
        self.len -= 1;
        Some((class, job))
    }

    /// A job of `class` ran for `took`.
    fn charge(&mut self, class: u64, took: Duration) {
        let floor = self.floor;
        let Some(c) = self.classes.get_mut(&class) else {
            return;
        };
        c.running -= 1;
        let nanos = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        let next = c
            .vtime
            .saturating_add(nanos.saturating_mul(WEIGHT_SCALE) / c.weight);
        if !c.jobs.is_empty() {
            self.ready.remove(&(c.vtime, class));
            self.ready.insert((next, class));
        }
        c.vtime = next;
        if c.jobs.is_empty() && c.running == 0 && c.vtime <= floor {
            // Nothing to remember: it would start from the floor anyway.
            self.classes.remove(&class);
        }
    }

    fn take_all(&mut self) -> VecDeque<Job> {
        let mut jobs = std::mem::take(&mut self.fifo);
        for c in self.classes.values_mut() {
            jobs.append(&mut c.jobs);
        }
        self.ready.clear();
        self.len = 0;
        jobs
    }
}

struct State {
    jobs: Queue,
    threads: usize,
    idle: usize,
    /// Jobs finished, for [`PoolStats`].
    completed: u64,
    /// No new job is taken: [`Pool::stop`] was called.
    stopped: bool,
}

/// What a pool is doing right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Threads alive, busy or idle.
    pub threads: usize,
    /// Threads running a job.
    pub busy: usize,
    /// Jobs waiting for a thread.
    pub queued: usize,
    /// Jobs finished since the pool was created. A job is counted just
    /// after its handle has been given its value.
    pub completed: u64,
    /// The most threads the pool may have.
    pub max_threads: usize,
}

pub(crate) struct Pool {
    state: Mutex<State>,
    work: Condvar,
    cfg: PoolConfig,
}

impl Pool {
    pub(crate) fn new(cfg: PoolConfig) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                jobs: Queue::default(),
                threads: 0,
                idle: 0,
                completed: 0,
                stopped: false,
            }),
            work: Condvar::new(),
            cfg: PoolConfig {
                max_threads: cfg.max_threads.max(1),
                ..cfg
            },
        })
    }

    /// Queues `job`. Gives it back if the pool was stopped.
    pub(crate) fn spawn(self: &Arc<Self>, job: Job) -> Result<(), Job> {
        self.spawn_if(job, |_| true)
    }

    /// Queues `job` in `class` with `weight` (a fair pool's share). Gives it
    /// back if the pool was stopped.
    pub(crate) fn spawn_in(self: &Arc<Self>, class: u64, weight: u32, job: Job) -> Result<(), Job> {
        self.queue(class, weight, job, |_| true)
    }

    /// Queues `job` if `admit(jobs already waiting)` agrees, under the same
    /// lock that queues it. Gives the job back if the pool was stopped or
    /// `admit` said no.
    pub(crate) fn spawn_if(
        self: &Arc<Self>,
        job: Job,
        admit: impl FnOnce(usize) -> bool,
    ) -> Result<(), Job> {
        self.queue(NO_CLASS, 1, job, admit)
    }

    fn queue(
        self: &Arc<Self>,
        class: u64,
        weight: u32,
        job: Job,
        admit: impl FnOnce(usize) -> bool,
    ) -> Result<(), Job> {
        let mut state = lock(&self.state);
        if state.stopped || !admit(state.jobs.len) {
            return Err(job);
        }
        state.jobs.push(self.cfg.fair, class, weight, job);
        // Each queued job needs a thread of its own. An idle thread is
        // counted as idle until it runs again, so jobs queued before it wakes
        // up must not all count on it: one that blocks would hold up the
        // rest.
        if state.jobs.len <= state.idle {
            drop(state);
            self.work.notify_one();
            return Ok(());
        }
        if state.threads >= self.cfg.max_threads {
            // Every thread is busy: the job waits its turn.
            return Ok(());
        }
        state.threads += 1;
        drop(state);
        let mut builder = thread::Builder::new().name(self.cfg.name.into());
        if let Some(size) = self.cfg.stack_size {
            builder = builder.stack_size(size);
        }
        let pool = self.clone();
        if builder.spawn(move || pool.work()).is_err() {
            let mut state = lock(&self.state);
            state.threads -= 1;
            if state.threads == 0 {
                // No thread will ever take them: dropping them cancels each
                // job's handle rather than leave it waiting.
                let jobs = state.jobs.take_all();
                drop(state);
                drop(jobs);
            }
        }
        Ok(())
    }

    fn work(self: Arc<Self>) {
        if self.cfg.nice != 0 {
            crate::sys::lower_thread_priority(self.cfg.nice);
        }
        let mut state = lock(&self.state);
        loop {
            if let Some((class, job)) = state.jobs.pop(self.cfg.fair) {
                drop(state);
                let start = self.cfg.fair.then(Instant::now);
                job();
                state = lock(&self.state);
                if let Some(start) = start {
                    state.jobs.charge(class, start.elapsed());
                }
                state.completed += 1;
                continue;
            }
            if state.stopped {
                state.threads -= 1;
                return;
            }
            state.idle += 1;
            let (next, timeout) = self
                .work
                .wait_timeout(state, self.cfg.idle)
                .unwrap_or_else(|e| e.into_inner());
            state = next;
            state.idle -= 1;
            if timeout.timed_out() && state.jobs.len == 0 {
                state.threads -= 1;
                return;
            }
        }
    }

    /// Takes no new job from now on and hands back the jobs still queued, so
    /// the caller can drop them outside the lock. Running jobs finish on
    /// their threads, which then end. Idempotent.
    pub(crate) fn stop(&self) -> VecDeque<Job> {
        let jobs = {
            let mut state = lock(&self.state);
            state.stopped = true;
            state.jobs.take_all()
        };
        self.work.notify_all();
        jobs
    }

    pub(crate) fn stats(&self) -> PoolStats {
        let state = lock(&self.state);
        PoolStats {
            threads: state.threads,
            busy: state.threads - state.idle,
            queued: state.jobs.len,
            completed: state.completed,
            max_threads: self.cfg.max_threads,
        }
    }
}
