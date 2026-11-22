//! The blocking threads of the epoll driver, and of the Windows driver for
//! the file operations Windows has no overlapped form of.
//!
//! A regular file is always "ready" as far as epoll is concerned, so a read
//! that has to wait for the disk would hold up the whole worker. The epoll
//! driver therefore hands file operations (opens, reads, writes, syncs,
//! metadata) to these threads, the way other readiness-based runtimes do.
//!
//! One pool serves the whole process. A thread starts whenever more jobs are
//! queued than idle threads can take, up to [`MAX_THREADS`], and stops after
//! [`IDLE`] without work: a job that blocks for a long time (a FIFO with no
//! writer, a hung network filesystem) holds up only its own thread. A
//! finished job pushes its result onto its worker's [`Completions`] and pokes
//! the worker's eventfd.
//!
//! When every thread is busy, jobs wait their turn. If no job finishes for
//! [`STALL`] while jobs wait, the busy threads are taken to be stuck, and a
//! thread starts for each waiting job, up to [`LIMIT`] threads in all:
//! [`MAX_THREADS`] stuck jobs then hold up nothing but themselves.

use std::collections::VecDeque;
#[cfg(unix)]
use std::io;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::{Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::runtime::lock;

/// The most blocking threads at once. Enough for disks that serve many
/// requests in parallel (NVMe), and a bound on what a hung network
/// filesystem can tie up.
pub(super) const MAX_THREADS: usize = 64;

/// The most blocking threads at once when [`MAX_THREADS`] of them are
/// stuck, as Tokio's blocking pool has.
const LIMIT: usize = 512;

/// How long jobs wait with every thread busy and none finishing before one
/// more thread starts. Disks under load finish something far more often,
/// and a job stuck for good waits this long once.
const STALL: Duration = Duration::from_millis(100);

/// How long a thread waits for work before it ends.
const IDLE: Duration = Duration::from_secs(10);

type Job = Box<dyn FnOnce() + Send>;

struct State {
    jobs: VecDeque<Job>,
    threads: usize,
    idle: usize,
    /// Jobs finished so far, so that the watcher can tell stuck threads
    /// from busy ones.
    done: u64,
    /// Set while a watcher thread runs.
    watching: bool,
}

struct Pool {
    state: Mutex<State>,
    work: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| Pool {
        state: Mutex::new(State {
            jobs: VecDeque::new(),
            threads: 0,
            idle: 0,
            done: 0,
            watching: false,
        }),
        work: Condvar::new(),
    })
}

/// Runs `job` on a blocking thread, or on the calling thread if no thread
/// can be started and none exists.
pub(super) fn spawn(job: Job) {
    let pool = pool();
    let mut state = lock(&pool.state);
    state.jobs.push_back(job);
    // Each queued job needs a thread of its own. An idle thread is counted
    // as idle until it runs again, so jobs queued before it wakes up must not
    // all count on it: one that blocks would hold up the rest.
    if state.jobs.len() <= state.idle {
        drop(state);
        pool.work.notify_one();
        return;
    }
    if state.threads >= MAX_THREADS {
        // Every thread is busy: the job waits its turn, and a watcher
        // starts another thread if the busy ones turn out to be stuck.
        if !state.watching {
            state.watching = true;
            drop(state);
            let started = thread::Builder::new()
                .name("giong-blocking-watch".into())
                .spawn(|| watch(self::pool()));
            if started.is_err() {
                lock(&pool.state).watching = false;
            }
        }
        return;
    }
    state.threads += 1;
    drop(state);
    if !start(pool) {
        let mut state = lock(&pool.state);
        state.threads -= 1;
        if state.threads == 0 {
            // No thread will ever take it: run what is queued here rather
            // than leave the operations hanging.
            let jobs = std::mem::take(&mut state.jobs);
            drop(state);
            for job in jobs {
                job();
            }
        }
    }
}

/// Starts a blocking thread, already counted in `threads`.
fn start(pool: &'static Pool) -> bool {
    thread::Builder::new()
        .name("giong-blocking".into())
        .spawn(move || work(pool))
        .is_ok()
}

fn work(pool: &'static Pool) {
    let mut state = lock(&pool.state);
    loop {
        if let Some(job) = state.jobs.pop_front() {
            drop(state);
            job();
            state = lock(&pool.state);
            state.done += 1;
            continue;
        }
        state.idle += 1;
        let (next, timeout) = pool
            .work
            .wait_timeout(state, IDLE)
            .unwrap_or_else(|e| e.into_inner());
        state = next;
        state.idle -= 1;
        if timeout.timed_out() && state.jobs.is_empty() {
            state.threads -= 1;
            return;
        }
    }
}

/// Runs while jobs wait with every thread busy. A [`STALL`] in which no job
/// finished means the busy threads are stuck (a FIFO with no writer, a hung
/// network filesystem): a thread starts for each waiting job, so the jobs
/// behind them run.
fn watch(pool: &'static Pool) {
    let mut seen = lock(&pool.state).done;
    loop {
        thread::sleep(STALL);
        let mut state = lock(&pool.state);
        if state.jobs.is_empty() {
            state.watching = false;
            return;
        }
        if state.done == seen {
            // A thread for each job that no idle thread will take: the
            // jobs behind the stuck ones then wait one stall, not one each.
            let more = (state.jobs.len().saturating_sub(state.idle))
                .min(LIMIT.saturating_sub(state.threads));
            state.threads += more;
            drop(state);
            let started = (0..more).take_while(|_| start(pool)).count();
            state = lock(&pool.state);
            state.threads -= more - started;
        }
        seen = state.done;
    }
}

/// Results of one worker's blocking jobs, waiting for the worker.
#[cfg(unix)]
pub(super) struct Completions {
    done: Mutex<Vec<(usize, i32)>>,
    /// Set while `done` is not empty, so the worker can check without the
    /// lock.
    any: AtomicBool,
    /// Rung after a push; the worker watches it for readability.
    bell: crate::sys::Bell,
}

#[cfg(unix)]
impl Completions {
    pub(super) fn new() -> io::Result<Arc<Self>> {
        let bell = crate::sys::Bell::new()?;
        Ok(Arc::new(Self {
            done: Mutex::new(Vec::new()),
            any: AtomicBool::new(false),
            bell,
        }))
    }

    pub(super) fn fd(&self) -> i32 {
        self.bell.read_fd()
    }

    /// Called on a blocking thread when the job for `key` is done.
    pub(super) fn push(&self, key: usize, res: i32) {
        lock(&self.done).push((key, res));
        self.any.store(true, Ordering::Release);
        // The `Arc` keeps the bell open while any job can still call this.
        self.bell.ring();
    }

    /// `true` if results are waiting; no lock, no system call.
    pub(super) fn any(&self) -> bool {
        self.any.load(Ordering::Acquire)
    }

    /// Resets the bell if no result is waiting. A job that finishes while
    /// [`take`](Self::take) runs can ring the bell after `take` reset it
    /// and took the job's result: the bell then stays readable with nothing
    /// to take, and a level-triggered watch would report it on every wait.
    /// A job's ring comes after its `any`, so a reset here never hides a
    /// result: `any` already says it.
    pub(super) fn quiet(&self) {
        if !self.any() {
            self.bell.drain();
        }
    }

    /// Rings the bell as a job does, with no result.
    #[cfg(test)]
    pub(super) fn ring(&self) {
        self.bell.ring();
    }

    /// Takes the waiting results, and resets the bell.
    pub(super) fn take(&self, into: &mut Vec<(usize, i32)>) {
        self.bell.drain();
        let mut done = lock(&self.done);
        self.any.store(false, Ordering::Relaxed);
        into.append(&mut done);
    }
}

/// Lets a blocking job see that it was cancelled before it started.
pub(super) struct Cancel(AtomicBool);

impl Cancel {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(false)))
    }

    pub(super) fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

// Not on Windows, where `iocp`'s tests keep every thread busy too: run at
// once, the two would take each other's threads, and the threads this test
// adds would leave that one an idle thread it does not expect.
#[cfg(all(test, unix, not(miri)))]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    #[test]
    fn a_job_behind_stuck_threads_still_runs() {
        // Every thread busy until `release`, and none finishing.
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new(AtomicUsize::new(0));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                *lock(&self.0 .0) = true;
                self.0 .1.notify_all();
            }
        }
        let release = Release(gate.clone());
        for _ in 0..MAX_THREADS {
            let (gate, started) = (gate.clone(), started.clone());
            spawn(Box::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                let mut open = lock(&gate.0);
                while !*open {
                    open = gate.1.wait(open).unwrap();
                }
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while started.load(Ordering::SeqCst) < MAX_THREADS {
            assert!(
                Instant::now() < deadline,
                "the blocking threads did not all start"
            );
            thread::sleep(Duration::from_millis(1));
        }
        let ran = Arc::new(AtomicBool::new(false));
        let start = Instant::now();
        spawn(Box::new({
            let ran = ran.clone();
            move || ran.store(true, Ordering::SeqCst)
        }));
        while !ran.load(Ordering::SeqCst) {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "a job behind {MAX_THREADS} stuck threads did not run"
            );
            thread::sleep(Duration::from_millis(1));
        }
        drop(release);
    }
}
