//! The single-threaded task executor that runs on every worker.
//!
//! Tasks are `!Send`: they are created, polled and dropped on one worker thread,
//! so they can freely use `Rc`/`RefCell`. Wakers, however, must be `Send + Sync`
//! by contract, so a waker never owns its task. It only carries the task's slot
//! index plus a handle to the worker's shared state: waking from the owning
//! thread pushes onto the local run queue; waking from any other thread goes
//! through the worker's injector and its eventfd.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::{poll_fn, Future};
use std::panic::{self, AssertUnwindSafe};
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;

use crate::notify::RunFlag;
use crate::observe::Location;
use crate::runtime::{self, lock, Inject, Worker, WorkerShared};

pub(crate) struct TaskSlot {
    /// `None` while the task is being polled.
    task: Option<Task>,
    /// Where the task was spawned (see [`crate::observe`]).
    location: Location,
    /// Unique per spawned task; guards [`JoinHandle::abort`] against slot reuse.
    uid: u64,
    /// Abort requested while the task was being polled.
    abort: bool,
}

struct Task {
    waker: Waker,
    state: Arc<TaskWaker>,
    fut: Pin<Box<dyn Future<Output = ()>>>,
}

pub(crate) struct TaskWaker {
    shared: Arc<WorkerShared>,
    id: usize,
    /// Set while the task sits in a run queue, so each wake enqueues it once.
    queued: RunFlag,
}

/// Run-queue id of the worker's main future, which lives on the stack of
/// `Worker::run` rather than in the task table.
pub(crate) const MAIN_TASK: usize = usize::MAX;

impl TaskWaker {
    /// The waker state of the main future (enqueued by the caller).
    pub(crate) fn main(shared: Arc<WorkerShared>) -> Arc<Self> {
        Arc::new(Self {
            shared,
            id: MAIN_TASK,
            queued: RunFlag::new(true),
        })
    }

    /// Called right before the task is polled (see [`RunFlag::begin_poll`]).
    pub(crate) fn clear_queued(&self) {
        self.queued.begin_poll();
    }
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.queued.try_enqueue() {
            return;
        }
        if runtime::current_uid() == self.shared.uid {
            // Same thread: straight onto the local run queue.
            runtime::try_with_worker(|w| w.queue.borrow_mut().push_back(self.id));
        } else {
            self.shared.inject(Inject::Wake(self.id));
        }
    }
}

/// Why a task did not produce a value.
pub enum JoinError {
    /// The task was aborted, or the runtime shut down before it finished.
    Cancelled,
    /// The task panicked; the payload is the panic value.
    Panic(Box<dyn Any + Send + 'static>),
}

impl JoinError {
    /// `true` if the task panicked.
    pub fn is_panic(&self) -> bool {
        matches!(self, JoinError::Panic(_))
    }

    /// `true` if the task was cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, JoinError::Cancelled)
    }

    /// Returns the panic payload, or `self` if the task did not panic.
    pub fn try_into_panic(self) -> Result<Box<dyn Any + Send + 'static>, JoinError> {
        match self {
            JoinError::Panic(payload) => Ok(payload),
            other => Err(other),
        }
    }
}

impl fmt::Debug for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinError::Cancelled => f.write_str("JoinError::Cancelled"),
            JoinError::Panic(_) => f.write_str("JoinError::Panic(..)"),
        }
    }
}

impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinError::Cancelled => f.write_str("task was cancelled"),
            JoinError::Panic(_) => f.write_str("task panicked"),
        }
    }
}

impl std::error::Error for JoinError {}

struct JoinState<T> {
    result: RefCell<Option<Result<T, JoinError>>>,
    waker: RefCell<Option<Waker>>,
    finished: Cell<bool>,
}

impl<T> JoinState<T> {
    fn complete(&self, result: Result<T, JoinError>) {
        if self.finished.replace(true) {
            return;
        }
        *self.result.borrow_mut() = Some(result);
        let waker = self.waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn is_finished(&self) -> bool {
        self.finished.get()
    }

    fn take(&self) -> Option<Result<T, JoinError>> {
        self.result.borrow_mut().take()
    }
}

/// Completes the join state as cancelled if the task is dropped unfinished.
struct CancelOnDrop<T>(Rc<JoinState<T>>);

impl<T> Drop for CancelOnDrop<T> {
    fn drop(&mut self) {
        self.0.complete(Err(JoinError::Cancelled));
    }
}

/// A handle to a spawned task. Awaiting it yields the task's output.
///
/// Dropping the handle detaches the task: it keeps running in the background.
pub struct JoinHandle<T> {
    state: Rc<JoinState<T>>,
    slot: usize,
    uid: u64,
    worker_uid: usize,
}

impl<T> JoinHandle<T> {
    /// Cancels the task: its future is dropped at the next opportunity and the
    /// handle resolves to [`JoinError::Cancelled`]. No-op if already finished.
    pub fn abort(&self) {
        if self.state.is_finished() {
            return;
        }
        let task = runtime::try_with_worker(|w| {
            if w.shared.uid != self.worker_uid {
                return None;
            }
            let mut tasks = w.tasks.borrow_mut();
            match tasks.get_mut(self.slot) {
                Some(slot) if slot.uid == self.uid => match slot.task.take() {
                    Some(task) => {
                        tasks.remove(self.slot);
                        Some(task)
                    }
                    None => {
                        // The task is aborting itself; the executor drops it
                        // as soon as the current poll returns.
                        slot.abort = true;
                        None
                    }
                },
                _ => None,
            }
        })
        .flatten();
        // Dropped outside of the task table borrow: destructors may spawn.
        drop_quietly(task);
    }

    /// `true` once the task has completed, panicked or been cancelled.
    pub fn is_finished(&self) -> bool {
        self.state.is_finished()
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.state.take() {
            return Poll::Ready(result);
        }
        assert!(
            !self.state.is_finished(),
            "JoinHandle polled after completion"
        );
        let mut waker = self.state.waker.borrow_mut();
        match &*waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => *waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl<T> fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

// -------------------------------------------------------------- remote tasks

/// What a [`RemoteJoinHandle`] shares with its task on another worker.
struct RemoteState<T> {
    /// Set by [`RemoteJoinHandle::abort`] (under `inner`'s lock); the task
    /// reads it before every poll.
    aborted: AtomicBool,
    inner: Mutex<RemoteInner<T>>,
}

struct RemoteInner<T> {
    result: Option<Result<T, JoinError>>,
    finished: bool,
    /// Wakes whoever awaits the handle.
    join_waker: Option<Waker>,
    /// Wakes the task, so that it sees an abort.
    task_waker: Option<Waker>,
}

impl<T> RemoteState<T> {
    fn complete(&self, result: Result<T, JoinError>) {
        let waker = {
            let mut inner = lock(&self.inner);
            if inner.finished {
                return;
            }
            inner.finished = true;
            inner.result = Some(result);
            inner.task_waker = None;
            inner.join_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Records the task's waker. Returns `true` if the task is aborted: the
    /// check runs under the same lock as [`RemoteJoinHandle::abort`], so an
    /// abort either sees this waker or is seen here.
    fn register(&self, waker: &Waker) -> bool {
        let mut inner = lock(&self.inner);
        inner.task_waker = Some(waker.clone());
        self.aborted.load(Ordering::Acquire)
    }
}

/// Completes the remote state as cancelled if it is dropped unfinished: the
/// worker refused the job or stopped before running it, or the task was
/// dropped (aborted, or the worker shut down).
struct RemoteGuard<T>(Arc<RemoteState<T>>);

impl<T> Drop for RemoteGuard<T> {
    fn drop(&mut self) {
        self.0.complete(Err(JoinError::Cancelled));
    }
}

/// A handle to a task started on another worker with
/// [`Handle::spawn_on`](crate::Handle::spawn_on). Awaiting it yields the
/// task's output.
///
/// Unlike [`JoinHandle`], it is `Send` and `Sync` (when the output is): await
/// it on any worker, or on a thread outside the runtime. The task itself never
/// leaves the worker it was started on.
///
/// Dropping the handle detaches the task: it keeps running in the background.
pub struct RemoteJoinHandle<T> {
    state: Arc<RemoteState<T>>,
}

impl<T: Send + 'static> RemoteJoinHandle<T> {
    /// The handle, and the job that the target worker runs to start the task.
    pub(crate) fn new<F, Fut>(f: F, location: Location) -> (Self, Box<dyn FnOnce() + Send>)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = T> + 'static,
    {
        let state = Arc::new(RemoteState {
            aborted: AtomicBool::new(false),
            inner: Mutex::new(RemoteInner {
                result: None,
                finished: false,
                join_waker: None,
                task_waker: None,
            }),
        });
        let guard = RemoteGuard(state.clone());
        let job = Box::new(move || {
            let guard = guard;
            if guard.0.aborted.load(Ordering::Acquire) {
                return;
            }
            match panic::catch_unwind(AssertUnwindSafe(f)) {
                Ok(fut) => drop(runtime::with_worker(|w| {
                    w.spawn_local(run_remote(fut, guard), location)
                })),
                Err(payload) => guard.0.complete(Err(JoinError::Panic(payload))),
            }
        });
        (Self { state }, job)
    }
}

impl<T> RemoteJoinHandle<T> {
    /// Cancels the task: its future is dropped on its own worker at the next
    /// opportunity, and the handle resolves to [`JoinError::Cancelled`]. Works
    /// from any thread. No-op if the task has already finished.
    pub fn abort(&self) {
        let waker = {
            let mut inner = lock(&self.state.inner);
            if inner.finished {
                return;
            }
            self.state.aborted.store(true, Ordering::Release);
            inner.task_waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// `true` once the task has completed, panicked or been cancelled.
    pub fn is_finished(&self) -> bool {
        lock(&self.state.inner).finished
    }
}

impl<T> Future for RemoteJoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = lock(&self.state.inner);
        if let Some(result) = inner.result.take() {
            return Poll::Ready(result);
        }
        assert!(!inner.finished, "RemoteJoinHandle polled after completion");
        match &inner.join_waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => inner.join_waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl<T> fmt::Debug for RemoteJoinHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteJoinHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

/// The task behind a [`RemoteJoinHandle`], on its own worker.
async fn run_remote<Fut: Future>(fut: Fut, guard: RemoteGuard<Fut::Output>) {
    let result = {
        let mut fut = pin!(fut);
        let mut registered: Option<Waker> = None;
        poll_fn(|cx| {
            let aborted = match &registered {
                Some(w) if w.will_wake(cx.waker()) => guard.0.aborted.load(Ordering::Acquire),
                _ => {
                    registered = Some(cx.waker().clone());
                    guard.0.register(cx.waker())
                }
            };
            if aborted {
                return Poll::Ready(Err(JoinError::Cancelled));
            }
            match panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
                Ok(Poll::Pending) => Poll::Pending,
                Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
                Err(payload) => Poll::Ready(Err(JoinError::Panic(payload))),
            }
        })
        .await
        // The user's future is dropped here, on its worker, before the handle
        // learns the result.
    };
    guard.0.complete(result);
}

/// Drops a value, swallowing a panic from its destructor so that one broken
/// task cannot take the whole worker thread down.
fn drop_quietly<T>(value: T) {
    let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(value)));
}

impl Worker {
    pub(crate) fn spawn_local<F>(&self, fut: F, location: Location) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
    {
        let state = Rc::new(JoinState {
            result: RefCell::new(None),
            waker: RefCell::new(None),
            finished: Cell::new(false),
        });
        let guard = CancelOnDrop(state.clone());
        let wrapped = Box::pin(async move {
            let guard = guard;
            let mut fut = pin!(fut);
            let result = poll_fn(|cx| {
                match panic::catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
                    Ok(Poll::Pending) => Poll::Pending,
                    Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
                    Err(payload) => Poll::Ready(Err(JoinError::Panic(payload))),
                }
            })
            .await;
            guard.0.complete(result);
        });

        let uid = self.next_task_uid.get();
        self.next_task_uid.set(uid + 1);

        let mut tasks = self.tasks.borrow_mut();
        let slot = tasks.insert(TaskSlot {
            task: None,
            location,
            uid,
            abort: false,
        });
        let waker_state = Arc::new(TaskWaker {
            shared: self.shared.clone(),
            id: slot,
            // Enqueued right below.
            queued: RunFlag::new(true),
        });
        let task = Task {
            waker: Waker::from(waker_state.clone()),
            state: waker_state,
            fut: wrapped,
        };
        tasks.get_mut(slot).expect("slot just inserted").task = Some(task);
        drop(tasks);
        self.queue.borrow_mut().push_back(slot);
        self.shared.counters.spawned();

        JoinHandle {
            state,
            slot,
            uid,
            worker_uid: self.shared.uid,
        }
    }

    /// Polls every task that was runnable when the call started. Tasks woken in
    /// the meantime wait for the next tick, so I/O is polled in between.
    pub(crate) fn run_tasks(&self, poll_main: &mut dyn FnMut()) {
        // Read once per batch, and two copies of the loop, so that profiling
        // turned off costs no clock reads and no branches per poll.
        if self.rt.profiling() {
            self.run_batch::<true>(poll_main);
        } else {
            self.run_batch::<false>(poll_main);
        }
        self.shared.counters.between_polls();
    }

    fn run_batch<const PROFILE: bool>(&self, poll_main: &mut dyn FnMut()) {
        let budget = self.queue.borrow().len();
        let counters = &self.shared.counters;
        for _ in 0..budget {
            let Some(id) = self.queue.borrow_mut().pop_front() else {
                break;
            };
            crate::driver::refill_budget();
            if id == MAIN_TASK {
                let location = self.main_location();
                counters.polling(location);
                let start = PROFILE.then(Instant::now);
                poll_main();
                if let Some(start) = start {
                    self.observe.record(location, start.elapsed());
                }
                continue;
            }
            let (task, location) = {
                let mut tasks = self.tasks.borrow_mut();
                let Some(slot) = tasks.get_mut(id) else {
                    continue;
                };
                (slot.task.take(), slot.location)
            };
            // A stale wake-up for a slot that is empty (or being polled).
            let Some(mut task) = task else { continue };

            task.state.clear_queued();
            counters.polling(location);
            let start = PROFILE.then(Instant::now);
            let finished = {
                let mut cx = Context::from_waker(&task.waker);
                // The task wrapper already catches panics from the user's future;
                // this guards the wrapper's own cleanup (destructors).
                match panic::catch_unwind(AssertUnwindSafe(|| task.fut.as_mut().poll(&mut cx))) {
                    Ok(Poll::Pending) => false,
                    Ok(Poll::Ready(())) | Err(_) => true,
                }
            };
            if let Some(start) = start {
                self.observe.record(location, start.elapsed());
            }

            let mut tasks = self.tasks.borrow_mut();
            let abort = tasks.get(id).map(|slot| slot.abort).unwrap_or(true);
            if finished || abort {
                tasks.remove(id);
                drop(tasks);
                drop_quietly(task);
            } else if let Some(slot) = tasks.get_mut(id) {
                slot.task = Some(task);
            }
        }
    }

    /// Where each live task was spawned, for [`crate::observe::snapshot`].
    pub(crate) fn task_locations(&self) -> Vec<Location> {
        self.tasks
            .borrow()
            .iter()
            .map(|slot| slot.location)
            .collect()
    }

    /// Drops every task. Destructors may spawn new tasks; those are dropped too.
    pub(crate) fn drop_all_tasks(&self) {
        loop {
            let slots = self.tasks.borrow_mut().take_all();
            if slots.is_empty() {
                break;
            }
            for slot in slots {
                drop_quietly(slot.task);
            }
        }
        self.queue.borrow_mut().clear();
    }
}

/// Spawns a task onto the current worker thread.
///
/// The future does not need to be `Send`: it never leaves this thread.
///
/// # Panics
///
/// Panics when called outside of a giong runtime.
#[track_caller]
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    let location = std::panic::Location::caller();
    runtime::with_worker(|w| w.spawn_local(fut, location))
}

/// Yields to the executor, letting other tasks and pending I/O make progress.
pub async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}
