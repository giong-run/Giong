//! A pool of plain threads that grows on demand and shrinks when idle.
//!
//! Three pools use it: the epoll driver's file pool
//! ([`driver::blocking`](crate::driver)), and the blocking and compute lanes
//! of [`crate::lane`]. A thread starts whenever more jobs are queued than idle
//! threads can take, up to the pool's limit, and ends after its idle time
//! without work: a job that blocks for a long time holds up only its own
//! thread.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

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
    /// Linux the nice value belongs to each thread.
    pub(crate) nice: i32,
    /// Put its threads in `SCHED_IDLE` (see [`crate::sched`]); `nice` is
    /// the fallback where the kernel refuses.
    pub(crate) idle_class: bool,
    /// Stack size of its threads; `None` for the standard library's default.
    pub(crate) stack_size: Option<usize>,
    /// When no thread can be started and none exists: `true` runs the
    /// queued jobs on the calling thread, `false` drops them.
    pub(crate) run_inline_on_failure: bool,
}

struct State {
    jobs: VecDeque<Job>,
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
    /// Jobs finished since the pool was created.
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
                jobs: VecDeque::new(),
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

    /// Queues `job` if `admit(jobs already waiting)` agrees, under the same
    /// lock that queues it. Gives the job back if the pool was stopped or
    /// `admit` said no.
    pub(crate) fn spawn_if(
        self: &Arc<Self>,
        job: Job,
        admit: impl FnOnce(usize) -> bool,
    ) -> Result<(), Job> {
        let mut state = lock(&self.state);
        if state.stopped || !admit(state.jobs.len()) {
            return Err(job);
        }
        state.jobs.push_back(job);
        // Each queued job needs a thread of its own. An idle thread is
        // counted as idle until it runs again, so jobs queued before it wakes
        // up must not all count on it: one that blocks would hold up the
        // rest.
        if state.jobs.len() <= state.idle {
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
                // No thread will ever take them. Run what is queued here
                // rather than leave the operations hanging, or drop it,
                // which cancels each job's handle.
                let jobs = std::mem::take(&mut state.jobs);
                drop(state);
                if self.cfg.run_inline_on_failure {
                    for job in jobs {
                        job();
                    }
                } else {
                    drop(jobs);
                }
            }
        }
        Ok(())
    }

    fn work(self: Arc<Self>) {
        if self.cfg.idle_class && crate::sched::set_idle_class() {
            // A waking worker now preempts this thread at once.
        } else if self.cfg.nice != 0 {
            // SAFETY: plain syscall with integer arguments. `who == 0` is the
            // calling thread on Linux, where each thread has its own nice
            // value. Raising it needs no privilege; failure leaves the
            // thread at its old priority, which is safe.
            unsafe {
                libc::setpriority(libc::PRIO_PROCESS, 0, self.cfg.nice);
            }
        }
        let mut state = lock(&self.state);
        loop {
            if let Some(job) = state.jobs.pop_front() {
                drop(state);
                job();
                state = lock(&self.state);
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
            if timeout.timed_out() && state.jobs.is_empty() {
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
            std::mem::take(&mut state.jobs)
        };
        self.work.notify_all();
        jobs
    }

    pub(crate) fn stats(&self) -> PoolStats {
        let state = lock(&self.state);
        PoolStats {
            threads: state.threads,
            busy: state.threads - state.idle,
            queued: state.jobs.len(),
            completed: state.completed,
            max_threads: self.cfg.max_threads,
        }
    }
}
