use std::fmt;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use super::context::Snapshot;
use super::deadline::poll_timer;
use super::ready::{MemberWaker, Ready};
use super::Cancelled;
use crate::executor::{JoinError, JoinHandle};
use crate::slab::Slab;
use crate::time::{sleep_until, Sleep};

/// A set of tasks owned together: results are collected as the tasks
/// finish, and every task still running is aborted when the group is
/// dropped.
///
/// The tasks are ordinary tasks on the current worker (`'static`, not
/// `Send`), so each one is polled and observed on its own. Each one
/// inherits the [deadline](super#deadlines) and the task-local values
/// in force where [`spawn`](Self::spawn) is called; a task whose inherited
/// deadline passes ends with [`JoinError::DeadlineExceeded`].
///
/// For futures that borrow local data, or when the first error should
/// cancel the rest, use a [`Scope`](super::Scope).
///
/// ```no_run
/// use giong::task::TaskGroup;
///
/// # async fn f(urls: Vec<String>) {
/// let mut group = TaskGroup::new();
/// for url in urls {
///     group.spawn(async move { url.len() });
/// }
/// while let Some(result) = group.join_next().await {
///     match result {
///         Ok(len) => println!("{len}"),
///         Err(e) => eprintln!("task failed: {e}"),
///     }
/// }
/// # }
/// ```
pub struct TaskGroup<T: 'static> {
    members: Slab<Member<T>>,
    ready: Arc<Ready>,
    woken: Vec<usize>,
}

struct Member<T: 'static> {
    handle: JoinHandle<Result<T, Cancelled>>,
    /// Set right after the member gets its slot, which its waker names.
    waker: Option<(Arc<MemberWaker>, Waker)>,
}

impl<T: 'static> TaskGroup<T> {
    /// An empty group.
    pub fn new() -> Self {
        Self {
            members: Slab::new(),
            ready: Arc::new(Ready::default()),
            woken: Vec::new(),
        }
    }

    /// Starts `fut` as a task on the current worker, in this group.
    ///
    /// # Panics
    ///
    /// Outside of a giong runtime, as [`spawn`](crate::spawn).
    #[track_caller]
    pub fn spawn<F>(&mut self, fut: F)
    where
        F: Future<Output = T> + 'static,
    {
        let snapshot = Snapshot::capture();
        let handle = crate::spawn(Captured {
            timer: snapshot.deadline.map(|d| sleep_until(d.at)),
            armed: None,
            snapshot,
            fut,
        });
        let id = self.members.insert(Member {
            handle,
            waker: None,
        });
        let member = self.members.get_mut(id).expect("just inserted");
        member.waker = Some(MemberWaker::new(id, &self.ready));
    }

    /// Waits for the next task to finish and returns its result, or `None`
    /// once the group is empty.
    ///
    /// Cancel safe: dropping the future loses no result; the task it would
    /// have returned stays in the group for the next call.
    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        poll_fn(|cx| self.poll_join_next(cx)).await
    }

    /// Polls for the next result: see [`join_next`](Self::join_next).
    pub fn poll_join_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<T, JoinError>>> {
        if self.members.len() == 0 {
            return Poll::Ready(None);
        }
        self.ready.set_parent(cx.waker());
        loop {
            if self.woken.is_empty() {
                self.ready.take(&mut self.woken);
                if self.woken.is_empty() {
                    return Poll::Pending;
                }
                // Oldest first.
                self.woken.reverse();
            }
            let Some(id) = self.woken.pop() else { continue };
            let Some(member) = self.members.get_mut(id) else {
                continue;
            };
            let Some((state, waker)) = &member.waker else {
                continue;
            };
            state.clear();
            let mut task_cx = Context::from_waker(waker);
            if let Poll::Ready(result) = Pin::new(&mut member.handle).poll(&mut task_cx) {
                self.members.remove(id);
                return Poll::Ready(Some(match result {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(_)) => Err(JoinError::DeadlineExceeded),
                    Err(e) => Err(e),
                }));
            }
        }
    }

    /// Aborts every task in the group. Their results still come out of
    /// [`join_next`](Self::join_next), as [`JoinError::Cancelled`] for the
    /// ones that had not finished.
    pub fn abort_all(&mut self) {
        for member in self.members.iter() {
            member.handle.abort();
        }
    }

    /// Aborts every task and forgets them: the group is empty afterwards.
    pub fn shutdown(&mut self) {
        self.abort_all();
        self.members.take_all();
        self.ready.clear();
        self.woken.clear();
    }

    /// The number of tasks whose result has not been taken yet.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// `true` if no task's result is waiting to be taken.
    pub fn is_empty(&self) -> bool {
        self.members.len() == 0
    }
}

impl<T: 'static> Default for TaskGroup<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: 'static> Drop for TaskGroup<T> {
    fn drop(&mut self) {
        self.abort_all();
    }
}

impl<T: 'static> fmt::Debug for TaskGroup<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskGroup")
            .field("len", &self.len())
            .finish()
    }
}

/// A task group's task: its future, run inside the context it was started
/// in.
struct Captured<F> {
    snapshot: Snapshot,
    timer: Option<Sleep>,
    armed: Option<Waker>,
    /// Pinned with the task: never moved out.
    fut: F,
}

impl<F: Future> Future for Captured<F> {
    type Output = Result<F::Output, Cancelled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `fut` is structurally pinned: it is only reached through
        // the pinned reference made here and never moved. The other fields
        // are `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        if let Some(d) = this.snapshot.deadline {
            if Instant::now() >= d.at {
                return Poll::Ready(Err(Cancelled::deadline()));
            }
        }
        // SAFETY: `fut` is structurally pinned (see above).
        let fut = unsafe { Pin::new_unchecked(&mut this.fut) };
        if let Poll::Ready(value) = this.snapshot.enter(|| fut.poll(cx)) {
            return Poll::Ready(Ok(value));
        }
        match &mut this.timer {
            Some(timer) => {
                poll_timer(timer, &mut this.armed, cx).map(|()| Err(Cancelled::deadline()))
            }
            None => Poll::Pending,
        }
    }
}
