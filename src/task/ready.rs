//! Which members of a scope or task group were woken, so that only those
//! are polled. Wakers may be called from any thread, hence the lock; every
//! member is polled on the worker that owns the scope or group.

use std::mem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Wake, Waker};

use crate::runtime::lock;

#[derive(Default)]
pub(crate) struct Ready {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    woken: Vec<usize>,
    /// The task that polls the scope or group.
    parent: Option<Waker>,
}

impl Ready {
    /// Records the waker of the task that polls the members.
    pub(crate) fn set_parent(&self, waker: &Waker) {
        let mut inner = lock(&self.inner);
        match &inner.parent {
            Some(w) if w.will_wake(waker) => {}
            _ => inner.parent = Some(waker.clone()),
        }
    }

    /// Marks member `id` as woken, and wakes the parent.
    pub(crate) fn push(&self, id: usize) {
        let parent = {
            let mut inner = lock(&self.inner);
            inner.woken.push(id);
            inner.parent.clone()
        };
        if let Some(parent) = parent {
            parent.wake();
        }
    }

    /// Wakes the parent without marking a member.
    pub(crate) fn wake_parent(&self) {
        let parent = lock(&self.inner).parent.clone();
        if let Some(parent) = parent {
            parent.wake();
        }
    }

    /// Takes the members woken so far into `out` (which it clears first).
    pub(crate) fn take(&self, out: &mut Vec<usize>) {
        out.clear();
        mem::swap(&mut lock(&self.inner).woken, out);
    }

    pub(crate) fn clear(&self) {
        lock(&self.inner).woken.clear();
    }
}

/// The waker of one member.
pub(crate) struct MemberWaker {
    id: usize,
    ready: Arc<Ready>,
    /// Set while the member's id is in `ready`, so each wake records it once.
    queued: AtomicBool,
}

impl MemberWaker {
    /// A waker for member `id`, already marked woken (it has to be polled
    /// a first time).
    pub(crate) fn new(id: usize, ready: &Arc<Ready>) -> (Arc<MemberWaker>, Waker) {
        let state = Arc::new(MemberWaker {
            id,
            ready: ready.clone(),
            queued: AtomicBool::new(true),
        });
        ready.push(id);
        let waker = Waker::from(state.clone());
        (state, waker)
    }

    /// Called right before the member is polled.
    pub(crate) fn clear(&self) {
        self.queued.store(false, Ordering::Release);
    }
}

impl Wake for MemberWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            self.ready.push(self.id);
        }
    }
}
