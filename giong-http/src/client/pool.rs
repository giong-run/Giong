//! The connections to one origin, handed out first come, first served.
//!
//! A request that finds no idle connection and no room to open one waits in
//! line. A connection that comes back goes to the first in line, and so does
//! the room left by one that closed: a request that has just finished cannot
//! jump the queue by asking again at once, so no request waits for long
//! while others are served.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use super::conn::Conn;

/// The connections to one origin.
#[derive(Default)]
pub(crate) struct HostPool {
    /// Connections not in use, the most recently used last. Only ever
    /// non-empty while nobody waits.
    idle: Vec<Conn>,
    /// Connections open, being opened, or in use.
    open: usize,
    waiters: VecDeque<Rc<Waiter>>,
}

pub(crate) type Shared = Rc<RefCell<HostPool>>;

#[derive(Default)]
struct Waiter {
    grant: RefCell<Option<Grant>>,
    waker: RefCell<Option<Waker>>,
}

enum Grant {
    Conn(Conn),
    /// Room to open a connection, already counted in `open`.
    Open,
}

/// What [`acquire`] got.
pub(crate) enum Taken {
    /// A connection that has carried requests before.
    Reused(Conn),
    /// Room for a new connection, counted in `open`: open one, and call
    /// [`free`] if that fails.
    Open,
}

impl HostPool {
    /// Open connections, and how many of them are idle.
    pub(crate) fn counts(&self) -> (usize, usize) {
        (self.open, self.idle.len())
    }

    /// The first waiter still waiting.
    fn next_waiter(&mut self) -> Option<Rc<Waiter>> {
        // A waiter whose request was dropped is held by the queue alone.
        while let Some(w) = self.waiters.pop_front() {
            if Rc::strong_count(&w) > 1 {
                return Some(w);
            }
        }
        None
    }
}

fn grant(waiter: &Waiter, grant: Grant) {
    *waiter.grant.borrow_mut() = Some(grant);
    if let Some(w) = waiter.waker.borrow_mut().take() {
        w.wake();
    }
}

/// A connection for one request: an idle one (unless `fresh`), or room to
/// open one, waiting in line when there is neither.
pub(crate) async fn acquire(
    pool: &Shared,
    max: usize,
    idle_timeout: Duration,
    fresh: bool,
) -> Taken {
    let waiter = {
        let mut p = pool.borrow_mut();
        if p.waiters.is_empty() {
            if fresh {
                // Make room by closing an idle connection, if need be.
                if p.open >= max && p.idle.pop().is_some() {
                    p.open -= 1;
                }
            } else {
                while let Some(conn) = p.idle.pop() {
                    if conn.idle_since.elapsed() >= idle_timeout || conn.is_spoiled() {
                        p.open -= 1;
                        continue;
                    }
                    return Taken::Reused(conn);
                }
            }
            if p.open < max {
                p.open += 1;
                return Taken::Open;
            }
        }
        let waiter = Rc::new(Waiter::default());
        p.waiters.push_back(waiter.clone());
        waiter
    };
    let guard = InLine { pool, waiter };
    let granted = poll_fn(|cx| match guard.waiter.grant.borrow_mut().take() {
        Some(g) => Poll::Ready(g),
        None => {
            *guard.waiter.waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        }
    })
    .await;
    drop(guard);
    match granted {
        // It finished a response a moment ago, so it is not checked for a
        // close the way an idle one is: should the server have closed it
        // since, the request finds out at once (see `Failure::NothingReceived`).
        Grant::Conn(conn) => Taken::Reused(conn),
        Grant::Open => Taken::Open,
    }
}

/// Puts a connection that can carry another request back: to the first in
/// line, or among the idle ones.
pub(crate) fn put_back(pool: &Shared, conn: Conn, idle_timeout: Duration) {
    let mut p = pool.borrow_mut();
    if let Some(w) = p.next_waiter() {
        drop(p);
        grant(&w, Grant::Conn(conn));
        return;
    }
    let before = p.idle.len();
    p.idle.retain(|c| c.idle_since.elapsed() < idle_timeout);
    p.open -= before - p.idle.len();
    p.idle.push(conn);
}

/// Closes the connections that have been idle for `idle_timeout`. Returns
/// whether the origin is left with no connection and nobody waiting.
pub(crate) fn sweep(pool: &Shared, idle_timeout: Duration) -> bool {
    let mut p = pool.borrow_mut();
    let before = p.idle.len();
    p.idle.retain(|c| c.idle_since.elapsed() < idle_timeout);
    p.open -= before - p.idle.len();
    p.open == 0 && p.waiters.is_empty()
}

/// Gives up a place: a connection closed, or one could not be opened. The
/// first in line may open one instead.
pub(crate) fn free(pool: &Shared) {
    let mut p = pool.borrow_mut();
    if let Some(w) = p.next_waiter() {
        drop(p);
        grant(&w, Grant::Open);
        return;
    }
    p.open -= 1;
}

/// A place in line. If the request is dropped after it was granted
/// something, the grant goes to the next in line.
struct InLine<'a> {
    pool: &'a Shared,
    waiter: Rc<Waiter>,
}

impl Drop for InLine<'_> {
    fn drop(&mut self) {
        let granted = self.waiter.grant.borrow_mut().take();
        match granted {
            Some(Grant::Conn(conn)) => put_back(self.pool, conn, Duration::MAX),
            Some(Grant::Open) => free(self.pool),
            None => self
                .pool
                .borrow_mut()
                .waiters
                .retain(|w| !Rc::ptr_eq(w, &self.waiter)),
        }
    }
}
