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
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use crate::notify::RunFlag;
use crate::runtime::{self, Inject, Worker, WorkerShared};

pub(crate) struct TaskSlot {
    /// `None` while the task is being polled.
    task: Option<Task>,
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

/// Drops a value, swallowing a panic from its destructor so that one broken
/// task cannot take the whole worker thread down.
fn drop_quietly<T>(value: T) {
    let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(value)));
}

impl Worker {
    pub(crate) fn spawn_local<F>(&self, fut: F) -> JoinHandle<F::Output>
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
        let budget = self.queue.borrow().len();
        for _ in 0..budget {
            let Some(id) = self.queue.borrow_mut().pop_front() else {
                break;
            };
            if id == MAIN_TASK {
                poll_main();
                continue;
            }
            let task = {
                let mut tasks = self.tasks.borrow_mut();
                tasks.get_mut(id).and_then(|slot| slot.task.take())
            };
            // A stale wake-up for a slot that is empty (or being polled).
            let Some(mut task) = task else { continue };

            task.state.clear_queued();
            let finished = {
                let mut cx = Context::from_waker(&task.waker);
                // The task wrapper already catches panics from the user's future;
                // this guards the wrapper's own cleanup (destructors).
                match panic::catch_unwind(AssertUnwindSafe(|| task.fut.as_mut().poll(&mut cx))) {
                    Ok(Poll::Pending) => false,
                    Ok(Poll::Ready(())) | Err(_) => true,
                }
            };

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
pub fn spawn<F>(fut: F) -> JoinHandle<F::Output>
where
    F: Future + 'static,
{
    runtime::with_worker(|w| w.spawn_local(fut))
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
