//! The two lock-free handshakes between a worker and the threads that wake its
//! tasks, kept apart from the rest of the runtime so that `loom` can check them
//! (`RUSTFLAGS="--cfg loom" cargo test --release --lib notify`).
//!
//! * [`RunFlag`]: whether a task already sits in a run queue, so that each
//!   wake-up enqueues it once, and so that a wake-up that skips the enqueue
//!   is still seen by the poll that follows.
//! * [`Doorbell`]: whether the worker has injected work to collect, and
//!   whether it is asleep in the kernel and must be woken through its eventfd.
//!
//! The models at the bottom drive these types with real threads under every
//! interleaving and every memory ordering outcome that loom explores, and fail
//! if a wake-up can be lost.

#[cfg(all(loom, test))]
use loom::sync::atomic::{fence, AtomicBool, Ordering};
#[cfg(not(all(loom, test)))]
use std::sync::atomic::{fence, AtomicBool, Ordering};

/// Run-queue membership of one task.
pub(crate) struct RunFlag(AtomicBool);

impl RunFlag {
    pub(crate) fn new(queued: bool) -> Self {
        Self(AtomicBool::new(queued))
    }

    /// Called by a waker after it has published the task's new state.
    /// Returns `true` if the caller must put the task in the run queue.
    pub(crate) fn try_enqueue(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }

    /// Called by the worker right before it polls the task.
    ///
    /// This must be a read-modify-write, not a plain store. A waker on another
    /// thread publishes the task's new state and then swaps the flag; if that
    /// swap still sees `true`, it relies on this poll to observe the new state.
    /// Only a swap guarantees that: it reads the waker's `true` and so
    /// synchronises with it. With a store the poll could read state from before
    /// the wake-up while the waker skipped the enqueue, and the task would
    /// sleep forever. On x86 that happened about 70 times per million
    /// cross-thread wake-ups of a task waiting on a lock-free flag; the
    /// `run_flag` model below finds it at once.
    pub(crate) fn begin_poll(&self) {
        self.0.swap(false, Ordering::AcqRel);
    }
}

/// Injected-work signalling between other threads and one worker: a
/// Dekker-style handshake in which at least one side always sees the other.
///
/// Each side stores its own flag, then issues a `SeqCst` fence, then reads the
/// other side's flag. The two fences are totally ordered, so whichever comes
/// second sees the store made before the first: either the worker sees
/// `pending` and does not sleep, or the waker sees `sleeping` and writes the
/// eventfd.
///
/// Two choices are there for loom's sake and cost nothing measurable (a park
/// is a system call anyway; the old code used `SeqCst` stores, which are an
/// `xchg` on x86 too):
///
/// * fences rather than `SeqCst` loads and stores: loom models `SeqCst`
///   fences exactly but treats `SeqCst` accesses as acquire/release;
/// * every write to the two flags is a swap: loom keeps only a partial
///   modification order between a plain store and a concurrent
///   read-modify-write, and with plain stores it reports a lost wake-up
///   that the C++ memory model rules out.
pub(crate) struct Doorbell {
    /// Something is waiting in the worker's injector.
    pending: AtomicBool,
    /// The worker is (about to be) blocked in the kernel.
    sleeping: AtomicBool,
}

impl Doorbell {
    pub(crate) fn new() -> Self {
        Self {
            pending: AtomicBool::new(false),
            sleeping: AtomicBool::new(false),
        }
    }

    /// Called by another thread after it has pushed work into the injector.
    /// Returns `true` if the worker is asleep and the caller must write its
    /// eventfd.
    pub(crate) fn ring(&self) -> bool {
        self.pending.swap(true, Ordering::Release);
        fence(Ordering::SeqCst);
        // Only one waker needs to write the eventfd: the swap elects it.
        self.sleeping.load(Ordering::Relaxed) && self.sleeping.swap(false, Ordering::Relaxed)
    }

    /// Called by the worker before it takes the injector's contents. Returns
    /// `false` if there is nothing to take.
    pub(crate) fn take(&self) -> bool {
        self.pending.swap(false, Ordering::Acquire)
    }

    /// A cheap look for a busy-polling worker; it re-checks with [`take`].
    ///
    /// [`take`]: Doorbell::take
    pub(crate) fn peek(&self) -> bool {
        self.pending.load(Ordering::Relaxed)
    }

    /// Called by the worker right before it blocks in the kernel. Returns
    /// `true` if work arrived in the meantime, in which case it must not block.
    pub(crate) fn prepare_park(&self) -> bool {
        self.sleeping.swap(true, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        self.pending.load(Ordering::Relaxed)
    }

    /// Called by the worker once it is back from the kernel.
    pub(crate) fn unpark(&self) {
        self.sleeping.swap(false, Ordering::Relaxed);
    }
}

#[cfg(all(test, loom))]
mod models {
    //! Each model ends with the worker blocked forever if a wake-up is lost;
    //! loom reports that as a deadlock.

    use super::*;
    use loom::sync::atomic::AtomicUsize;
    use loom::sync::{Arc, Condvar, Mutex};
    use loom::thread;

    /// A counting eventfd: `write` adds, `read` blocks until non-zero and
    /// takes everything.
    struct EventFd {
        count: Mutex<u64>,
        ready: Condvar,
    }

    impl EventFd {
        fn new() -> Self {
            Self {
                count: Mutex::new(0),
                ready: Condvar::new(),
            }
        }

        fn write(&self) {
            *self.count.lock().unwrap() += 1;
            self.ready.notify_one();
        }

        fn read(&self) {
            let mut count = self.count.lock().unwrap();
            while *count == 0 {
                count = self.ready.wait(count).unwrap();
            }
            *count = 0;
        }
    }

    /// The worker side of `Worker::drive` and `WorkerShared::inject`, reduced
    /// to the doorbell: collect injected items until `want` have arrived,
    /// sleeping in the "kernel" (the eventfd) whenever there is nothing to do.
    fn worker(bell: &Doorbell, injector: &Mutex<Vec<usize>>, eventfd: &EventFd, want: usize) {
        let mut got = 0;
        loop {
            if bell.take() {
                got += std::mem::take(&mut *injector.lock().unwrap()).len();
            }
            if got == want {
                return;
            }
            if !bell.prepare_park() {
                eventfd.read();
            }
            bell.unpark();
        }
    }

    fn inject(bell: &Doorbell, injector: &Mutex<Vec<usize>>, eventfd: &EventFd, item: usize) {
        injector.lock().unwrap().push(item);
        if bell.ring() {
            eventfd.write();
        }
    }

    #[test]
    fn doorbell_one_injector() {
        loom::model(|| {
            let bell = Arc::new(Doorbell::new());
            let injector = Arc::new(Mutex::new(Vec::new()));
            let eventfd = Arc::new(EventFd::new());
            let t = {
                let (bell, injector, eventfd) = (bell.clone(), injector.clone(), eventfd.clone());
                thread::spawn(move || {
                    inject(&bell, &injector, &eventfd, 1);
                    inject(&bell, &injector, &eventfd, 2);
                })
            };
            worker(&bell, &injector, &eventfd, 2);
            t.join().unwrap();
        });
    }

    #[test]
    fn doorbell_two_injectors() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| {
            let bell = Arc::new(Doorbell::new());
            let injector = Arc::new(Mutex::new(Vec::new()));
            let eventfd = Arc::new(EventFd::new());
            let threads: Vec<_> = (0..2)
                .map(|i| {
                    let (bell, injector, eventfd) =
                        (bell.clone(), injector.clone(), eventfd.clone());
                    thread::spawn(move || inject(&bell, &injector, &eventfd, i))
                })
                .collect();
            worker(&bell, &injector, &eventfd, 2);
            for t in threads {
                t.join().unwrap();
            }
        });
    }

    /// A run queue the worker blocks on when it is empty.
    struct RunQueue {
        len: Mutex<usize>,
        ready: Condvar,
    }

    impl RunQueue {
        fn push(&self) {
            *self.len.lock().unwrap() += 1;
            self.ready.notify_one();
        }

        fn pop(&self) {
            let mut len = self.len.lock().unwrap();
            while *len == 0 {
                len = self.ready.wait(len).unwrap();
            }
            *len -= 1;
        }
    }

    /// A task whose state is a lock-free counter, woken from other threads
    /// the way `TaskWaker::wake_by_ref` does it. Each waker publishes its
    /// event and then wakes; the worker polls until it has seen every event.
    fn run_flag_model(wakers: usize) {
        let flag = Arc::new(RunFlag::new(true));
        let events = Arc::new(AtomicUsize::new(0));
        let queue = Arc::new(RunQueue {
            len: Mutex::new(1),
            ready: Condvar::new(),
        });
        let threads: Vec<_> = (0..wakers)
            .map(|_| {
                let (flag, events, queue) = (flag.clone(), events.clone(), queue.clone());
                thread::spawn(move || {
                    events.fetch_add(1, Ordering::Release);
                    if flag.try_enqueue() {
                        queue.push();
                    }
                })
            })
            .collect();
        loop {
            queue.pop();
            flag.begin_poll();
            // The poll: the task is done once it has seen every event.
            if events.load(Ordering::Acquire) == wakers {
                break;
            }
        }
        for t in threads {
            t.join().unwrap();
        }
    }

    #[test]
    fn run_flag_one_waker() {
        loom::model(|| run_flag_model(1));
    }

    #[test]
    fn run_flag_two_wakers() {
        let mut builder = loom::model::Builder::new();
        builder.preemption_bound = Some(3);
        builder.check(|| run_flag_model(2));
    }
}
