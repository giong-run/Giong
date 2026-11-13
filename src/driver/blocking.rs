//! The blocking threads of the epoll driver.
//!
//! A regular file is always "ready" as far as epoll is concerned, so a read
//! that has to wait for the disk would hold up the whole worker. The epoll
//! driver therefore hands file operations (opens, reads, writes, syncs,
//! metadata) to these threads, the way other readiness-based runtimes do.
//!
//! One pool serves the whole process. Threads start when work arrives and
//! nobody is idle, up to [`MAX_THREADS`], and stop after [`IDLE`] without
//! work. A finished job pushes its result onto its worker's [`Completions`]
//! and pokes the worker's eventfd.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use crate::runtime::lock;

/// The most blocking threads at once. Enough for disks that serve many
/// requests in parallel (NVMe), and a bound on what a hung network
/// filesystem can tie up.
const MAX_THREADS: usize = 64;

/// How long a thread waits for work before it ends.
const IDLE: Duration = Duration::from_secs(10);

type Job = Box<dyn FnOnce() + Send>;

struct State {
    jobs: VecDeque<Job>,
    threads: usize,
    idle: usize,
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
    if state.idle > 0 {
        drop(state);
        pool.work.notify_one();
        return;
    }
    if state.threads >= MAX_THREADS {
        // Every thread is busy: the job waits its turn.
        return;
    }
    state.threads += 1;
    drop(state);
    let started = thread::Builder::new()
        .name("giong-blocking".into())
        .spawn(|| work(self::pool()));
    if started.is_err() {
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

fn work(pool: &'static Pool) {
    let mut state = lock(&pool.state);
    loop {
        if let Some(job) = state.jobs.pop_front() {
            drop(state);
            job();
            state = lock(&pool.state);
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

/// Results of one worker's blocking jobs, waiting for the worker.
pub(super) struct Completions {
    done: Mutex<Vec<(usize, i32)>>,
    /// Set while `done` is not empty, so the worker can check without the
    /// lock.
    any: AtomicBool,
    /// Poked after a push; the worker watches it with epoll.
    eventfd: OwnedFd,
}

impl Completions {
    pub(super) fn new() -> io::Result<Arc<Self>> {
        // SAFETY: plain syscall; the result is checked.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Arc::new(Self {
            done: Mutex::new(Vec::new()),
            any: AtomicBool::new(false),
            // SAFETY: a fresh descriptor we own.
            eventfd: unsafe { OwnedFd::from_raw_fd(fd) },
        }))
    }

    pub(super) fn fd(&self) -> i32 {
        self.eventfd.as_raw_fd()
    }

    /// Called on a blocking thread when the job for `key` is done.
    pub(super) fn push(&self, key: usize, res: i32) {
        lock(&self.done).push((key, res));
        self.any.store(true, Ordering::Release);
        let one: u64 = 1;
        // SAFETY: writes 8 bytes from a valid u64 to an eventfd we own (the
        // `Arc` keeps it open while any job can still call this).
        unsafe {
            libc::write(
                self.eventfd.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            );
        }
    }

    /// `true` if results are waiting; no lock, no system call.
    pub(super) fn any(&self) -> bool {
        self.any.load(Ordering::Acquire)
    }

    /// Takes the waiting results, and resets the eventfd.
    pub(super) fn take(&self, into: &mut Vec<(usize, i32)>) {
        let mut count: u64 = 0;
        // SAFETY: reads 8 bytes into a valid u64; the eventfd is
        // non-blocking, so an empty one returns EAGAIN at once.
        unsafe {
            libc::read(
                self.eventfd.as_raw_fd(),
                &mut count as *mut u64 as *mut libc::c_void,
                8,
            );
        }
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
