use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use super::context::{self, DeadlineAt};
use super::deadline::{after, poll_timer};
use super::ready::{MemberWaker, Ready};
use super::Cancelled;
use crate::error::{Code, Error};
use crate::slab::Slab;
use crate::time::{sleep_until, Sleep};

/// Futures that run at the same time as children of one call: none
/// outlives [`run`](Self::run), the first error cancels the rest, and the
/// children may borrow local data.
///
/// ```no_run
/// use giong::task::Scope;
///
/// # async fn fetch(url: &str) -> std::io::Result<Vec<u8>> { Ok(Vec::new()) }
/// # async fn f() -> std::io::Result<()> {
/// let urls = vec!["a".to_string(), "b".to_string()];
/// let scope = Scope::new();
/// let pages = scope
///     .run(async {
///         // Children borrow `urls`: no `'static`, no `clone`.
///         let a = scope.spawn(fetch(&urls[0]));
///         let b = scope.spawn(fetch(&urls[1]));
///         Ok((a.await, b.await))
///     })
///     .await?;
/// # let _ = pages;
/// # Ok(())
/// # }
/// ```
///
/// The rules:
///
/// * [`run`](Self::run) polls its body and every child, on the current
///   task, and returns once the body has returned **and** every child has
///   finished.
/// * A child or the body that returns `Err(e)` cancels the scope: every
///   other child and the body are dropped (which cancels their I/O), and
///   `run` returns `Err(e)`. The first error wins.
/// * [`cancel`](Self::cancel) drops every child; `run` returns the body's
///   value if the body had already returned, else
///   `Err(`[`Cancelled`]`)` converted into `E`.
/// * A scope made with [`with_deadline`](Self::with_deadline) or
///   [`with_timeout`](Self::with_timeout) runs everything inside under
///   that deadline (see the [module docs](super#deadlines)), and `run`
///   returns the deadline's `Cancelled` converted into `E` when it passes.
///   A scope with a deadline and no children is how a piece of work gets a
///   deadline:
///   `Scope::with_timeout(d).run(async { .. }).await?`.
/// * A panic in a child or the body unwinds out of `run`, as one in the
///   body itself would; the other children are dropped with the scope.
/// * Children see the deadline and the task-local values in force around
///   `run`. Children only run while `run` is awaited.
/// * The body may borrow the scope; a child cannot, since it is stored in
///   the scope. A child that needs to cancel the scope keeps a
///   [`cancel_handle`](Self::cancel_handle); one that fans out further
///   opens a `Scope` of its own.
///
/// `E` is the error type of the body and every child, [`io::Error`] unless
/// said otherwise. It must convert from [`Cancelled`], as `io::Error`
/// does (`GIONG-E0024`, `GIONG-E0025`).
///
/// Everything runs on one task, so children never run in parallel with
/// each other; they run concurrently, as any futures on one giong worker
/// do. To spread work over workers, start it with
/// [`Handle::spawn_on`](crate::Handle::spawn_on) and await the handles
/// from children.
pub struct Scope<'env, E: 'env = io::Error> {
    shared: Rc<Shared<E>>,
    children: RefCell<Slab<Child<'env>>>,
    deadline: Option<Instant>,
    running: Cell<bool>,
}

struct Child<'env> {
    /// `None` while it is being polled.
    fut: Option<Pin<Box<dyn Future<Output = ()> + 'env>>>,
    /// Set right after the child gets its slot, which its waker names.
    waker: Option<(Arc<MemberWaker>, Waker)>,
}

struct Shared<E> {
    flags: Rc<Flags>,
    /// The first error.
    failure: RefCell<Option<E>>,
}

/// The part of a scope's state that does not depend on its error type,
/// shared with its [`CancelHandle`]s.
struct Flags {
    ready: Arc<Ready>,
    cancelled: Cell<bool>,
    failed: Cell<bool>,
}

impl Flags {
    fn cancel(&self) {
        self.cancelled.set(true);
        self.ready.wake_parent();
    }

    fn stopped(&self) -> bool {
        self.cancelled.get() || self.failed.get()
    }
}

impl<E> Shared<E> {
    fn fail(&self, e: E) {
        let mut failure = self.failure.borrow_mut();
        if failure.is_none() && !self.flags.failed.get() {
            *failure = Some(e);
            drop(failure);
            self.flags.failed.set(true);
            self.flags.ready.wake_parent();
        }
    }

    fn stopped(&self) -> bool {
        self.flags.stopped()
    }
}

/// The run-queue id of the body among the children.
const BODY: usize = usize::MAX;

impl<'env, E: 'env> Scope<'env, E> {
    /// A scope without a deadline of its own (one in force around
    /// [`run`](Self::run) still holds).
    pub fn new() -> Self {
        Self {
            shared: Rc::new(Shared {
                flags: Rc::new(Flags {
                    ready: Arc::new(Ready::default()),
                    cancelled: Cell::new(false),
                    failed: Cell::new(false),
                }),
                failure: RefCell::new(None),
            }),
            children: RefCell::new(Slab::new()),
            deadline: None,
            running: Cell::new(false),
        }
    }

    /// A scope whose [`run`](Self::run) ends at `at`.
    pub fn with_deadline(at: Instant) -> Self {
        let mut scope = Self::new();
        scope.deadline = Some(at);
        scope
    }

    /// A scope whose [`run`](Self::run) ends `duration` after this call.
    pub fn with_timeout(duration: Duration) -> Self {
        Self::with_deadline(after(duration))
    }

    /// Starts `fut` as a child of the scope. Its `Ok` value comes out of
    /// the returned handle; an `Err` cancels the scope (see the type docs).
    pub fn spawn<T, F>(&self, fut: F) -> ScopeHandle<T>
    where
        T: 'env,
        F: Future<Output = Result<T, E>> + 'env,
    {
        let slot = Rc::new(Slot {
            value: RefCell::new(None),
            waker: RefCell::new(None),
            done: Cell::new(false),
        });
        let child_slot = slot.clone();
        let shared = self.shared.clone();
        let fut = Box::pin(async move {
            match fut.await {
                Ok(value) => child_slot.set(value),
                Err(e) => shared.fail(e),
            }
        });
        let mut children = self.children.borrow_mut();
        let id = children.insert(Child {
            fut: Some(fut),
            waker: None,
        });
        children.get_mut(id).expect("just inserted").waker =
            Some(MemberWaker::new(id, &self.shared.flags.ready));
        ScopeHandle { slot }
    }

    /// Cancels the scope: every child is dropped at the next poll of
    /// [`run`](Self::run), and so is the body if it has not returned yet.
    pub fn cancel(&self) {
        self.shared.flags.cancel();
    }

    /// A handle that cancels this scope, for a child to keep: a child
    /// cannot borrow the scope it runs in (it is stored inside it).
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            flags: self.shared.flags.clone(),
        }
    }

    /// `true` once [`cancel`](Self::cancel) was called or a child or the
    /// body returned an error, during the current or last run.
    pub fn is_cancelled(&self) -> bool {
        self.shared.stopped()
    }

    /// Runs `body` and every child until all have finished: see the type
    /// docs.
    ///
    /// # Panics
    ///
    /// With [`GIONG-E0026`](crate::error::Code::ScopeAlreadyRunning) when
    /// the scope is already being run by another `run` future.
    pub fn run<'s, T, B>(&'s self, body: B) -> Run<'s, 'env, E, T, B>
    where
        B: Future<Output = Result<T, E>>,
        E: From<Cancelled>,
    {
        Run {
            scope: self,
            body: Some(body),
            output: None,
            timer: None,
            armed: None,
            kernel: None,
            started: false,
            woken: Vec::new(),
            body_waker: None,
        }
    }

    /// Drops every child (outside of the borrow: their destructors may use
    /// the scope).
    fn drop_children(&self) {
        let children = self.children.borrow_mut().take_all();
        drop(children);
        self.shared.flags.ready.clear();
    }
}

impl<'env, E: 'env> Default for Scope<'env, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'env, E: 'env> fmt::Debug for Scope<'env, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scope")
            .field("children", &self.children.borrow().len())
            .field("deadline", &self.deadline)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Cancels the [`Scope`] it came from, like [`Scope::cancel`]: see
/// [`Scope::cancel_handle`].
#[derive(Clone)]
pub struct CancelHandle {
    flags: Rc<Flags>,
}

impl CancelHandle {
    /// Cancels the scope.
    pub fn cancel(&self) {
        self.flags.cancel();
    }

    /// `true` once the scope was cancelled or failed.
    pub fn is_cancelled(&self) -> bool {
        self.flags.stopped()
    }
}

impl fmt::Debug for CancelHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelHandle")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// What a child shares with its [`ScopeHandle`].
struct Slot<T> {
    value: RefCell<Option<T>>,
    waker: RefCell<Option<Waker>>,
    done: Cell<bool>,
}

impl<T> Slot<T> {
    fn set(&self, value: T) {
        *self.value.borrow_mut() = Some(value);
        self.done.set(true);
        let waker = self.waker.borrow_mut().take();
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// The handle of a child started with [`Scope::spawn`]. Awaiting it gives
/// the child's `Ok` value. (An `Err` cancels the scope, the body with it,
/// so the body never sees one.)
#[must_use = "a child runs whether or not its handle is awaited; drop the handle if its value is not needed"]
pub struct ScopeHandle<T> {
    slot: Rc<Slot<T>>,
}

impl<T> ScopeHandle<T> {
    /// `true` once the child has returned its value.
    pub fn is_finished(&self) -> bool {
        self.slot.done.get()
    }
}

impl<T> Future for ScopeHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if let Some(value) = self.slot.value.borrow_mut().take() {
            return Poll::Ready(value);
        }
        assert!(
            !self.slot.done.get(),
            "ScopeHandle polled after it returned its value"
        );
        let mut waker = self.slot.waker.borrow_mut();
        match &*waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => *waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

impl<T> fmt::Debug for ScopeHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScopeHandle")
            .field("finished", &self.is_finished())
            .finish()
    }
}

/// Future returned by [`Scope::run`].
#[must_use = "futures do nothing unless awaited"]
pub struct Run<'s, 'env, E: 'env, T, B> {
    scope: &'s Scope<'env, E>,
    /// Pinned with the `Run`; `None` once it has returned or was dropped.
    body: Option<B>,
    /// The body's value, until every child has finished.
    output: Option<T>,
    timer: Option<Sleep>,
    armed: Option<Waker>,
    kernel: Option<DeadlineAt>,
    started: bool,
    woken: Vec<usize>,
    /// The body's waker: it marks the body as woken, like a child's.
    body_waker: Option<(Arc<MemberWaker>, Waker)>,
}

impl<'s, 'env, E: 'env, T, B> Run<'s, 'env, E, T, B> {
    /// Drops the body and every child, and ends the run.
    fn finish(self: Pin<&mut Self>) {
        // SAFETY: `body` is structurally pinned; it is dropped in place by
        // `Pin::set` and never moved. The other fields are not pinned.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: as above.
        unsafe { Pin::new_unchecked(&mut this.body) }.set(None);
        this.scope.drop_children();
        if this.started {
            this.started = false;
            this.scope.running.set(false);
        }
    }
}

impl<'s, 'env, E, T, B> Future for Run<'s, 'env, E, T, B>
where
    E: From<Cancelled> + 'env,
    B: Future<Output = Result<T, E>>,
{
    type Output = Result<T, E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<T, E>> {
        let scope = self.scope;
        let shared = &*scope.shared;
        if !self.started {
            if scope.running.replace(true) {
                already_running();
            }
            // SAFETY: only unpinned fields are written.
            let this = unsafe { self.as_mut().get_unchecked_mut() };
            this.started = true;
            shared.failure.borrow_mut().take();
            shared.flags.cancelled.set(false);
            shared.flags.failed.set(false);
            this.body_waker = Some(MemberWaker::new(BODY, &shared.flags.ready));
        }
        if let Some(at) = scope.deadline {
            if Instant::now() >= at {
                self.as_mut().finish();
                return Poll::Ready(Err(E::from(Cancelled::deadline())));
            }
        }
        shared.flags.ready.set_parent(cx.waker());
        {
            // SAFETY: `body` is structurally pinned (see `finish`); the other
            // fields are not.
            let this = unsafe { self.as_mut().get_unchecked_mut() };
            let _deadline = scope.deadline.map(|at| {
                context::enter_deadline(*this.kernel.get_or_insert_with(|| DeadlineAt::new(at)))
            });
            shared.flags.ready.take(&mut this.woken);
            // Oldest first; members woken while these run wait for the
            // next poll, so a child that keeps waking itself cannot hold the
            // others back.
            this.woken.reverse();
            while let Some(id) = this.woken.pop() {
                if shared.stopped() {
                    break;
                }
                if id == BODY {
                    // SAFETY: as above.
                    let mut body = unsafe { Pin::new_unchecked(&mut this.body) };
                    let Some(fut) = body.as_mut().as_pin_mut() else {
                        continue;
                    };
                    let Some((state, waker)) = &this.body_waker else {
                        continue;
                    };
                    state.clear();
                    match fut.poll(&mut Context::from_waker(waker)) {
                        Poll::Ready(Ok(value)) => {
                            this.output = Some(value);
                            body.set(None);
                        }
                        Poll::Ready(Err(e)) => {
                            body.set(None);
                            shared.fail(e);
                        }
                        Poll::Pending => {}
                    }
                    continue;
                }
                let taken = {
                    let mut children = scope.children.borrow_mut();
                    children.get_mut(id).and_then(|child| {
                        let (state, waker) = child.waker.as_ref()?;
                        let fut = child.fut.take()?;
                        state.clear();
                        Some((fut, waker.clone()))
                    })
                };
                let Some((mut fut, waker)) = taken else {
                    continue;
                };
                let done = fut
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_ready();
                let mut children = scope.children.borrow_mut();
                if done {
                    children.remove(id);
                    drop(children);
                    drop(fut);
                } else if let Some(child) = children.get_mut(id) {
                    child.fut = Some(fut);
                }
            }
        }
        let failure = shared.failure.borrow_mut().take();
        if let Some(e) = failure {
            self.as_mut().finish();
            return Poll::Ready(Err(e));
        }
        if shared.flags.cancelled.get() {
            // SAFETY: `output` is not pinned.
            let output = unsafe { self.as_mut().get_unchecked_mut() }.output.take();
            self.as_mut().finish();
            return Poll::Ready(output.ok_or_else(|| E::from(Cancelled::requested())));
        }
        if self.body.is_none() && scope.children.borrow().len() == 0 {
            // SAFETY: `output` is not pinned.
            let output = unsafe { self.as_mut().get_unchecked_mut() }.output.take();
            self.as_mut().finish();
            if let Some(output) = output {
                return Poll::Ready(Ok(output));
            }
            // Only a run that was already finished gets here.
            panic!("Scope::run polled after it returned");
        }
        if let Some(at) = scope.deadline {
            // SAFETY: `timer` is not pinned.
            let this = unsafe { self.as_mut().get_unchecked_mut() };
            let timer = this.timer.get_or_insert_with(|| sleep_until(at));
            if poll_timer(timer, &mut this.armed, cx).is_ready() {
                self.as_mut().finish();
                return Poll::Ready(Err(E::from(Cancelled::deadline())));
            }
        }
        Poll::Pending
    }
}

impl<'s, 'env, E: 'env, T, B> Drop for Run<'s, 'env, E, T, B> {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: `drop` is the last use of `self`, which is pinned for
            // its whole life: `finish` drops the body in place.
            unsafe { Pin::new_unchecked(self) }.finish();
        }
    }
}

impl<'s, 'env, E: 'env, T, B> fmt::Debug for Run<'s, 'env, E, T, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Run")
            .field("scope", self.scope)
            .finish_non_exhaustive()
    }
}

#[cold]
#[track_caller]
fn already_running() -> ! {
    panic!(
        "{}",
        Error::new(
            Code::ScopeAlreadyRunning,
            "Scope::run was polled while another run of the same scope is still going",
            "await one run of a scope at a time. To run more work in it, call scope.spawn from \
             the body or a child; for independent groups of work, make a Scope for each.",
        )
    )
}
