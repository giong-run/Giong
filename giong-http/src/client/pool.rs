//! The connections to one origin, handed out first come, first served.
//!
//! A request that finds no idle connection and no room to open one waits in
//! line. A connection that comes back goes to the first in line, and so does
//! the room left by one that closed: a request that has just finished cannot
//! jump the queue by asking again at once, so no request waits for long
//! while others are served.
//!
//! An HTTP/2 connection is not handed out: it stays in the pool and every
//! request that finds room on it sends its stream there. Once an origin is
//! known to speak HTTP/2, a request that would open a connection while one
//! is being opened waits for it instead, so that a burst of requests shares
//! one connection rather than opening one each.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::poll_fn;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use super::conn::Conn;
use super::h2;

/// The connections to one origin.
#[derive(Default)]
pub(crate) struct HostPool {
    /// Connections not in use, the most recently used last. Only ever
    /// non-empty while nobody waits.
    idle: Vec<Conn>,
    /// Connections open, being opened, or in use.
    open: usize,
    waiters: VecDeque<Rc<Waiter>>,
    /// HTTP/2 connections, each counted in `open`, with an identifier.
    h2: Vec<(u64, h2::Connection)>,
    next_h2: u64,
    /// The origin chose HTTP/2 the last time a connection was opened.
    h2_known: bool,
    /// A connection is being opened to an origin known to speak HTTP/2.
    h2_opening: bool,
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
    /// Look again: an HTTP/2 connection has room now.
    Retry,
}

/// What [`acquire`] got.
pub(crate) enum Taken {
    /// A connection that has carried requests before.
    Reused(Conn),
    /// Room for a new connection, counted in `open`: open one, and call
    /// [`free`] if that fails.
    Open,
    /// An HTTP/2 connection may have room now: look again.
    Retry,
}

impl HostPool {
    /// Open connections, and how many of them are idle.
    pub(crate) fn counts(&self) -> (usize, usize) {
        let idle_h2 = self.h2.iter().filter(|(_, c)| c.streams() == 0).count();
        (self.open, self.idle.len() + idle_h2)
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
/// open one, waiting in line when there is neither. `h2` says the origin
/// is known to speak HTTP/2 before any connection is opened (prior
/// knowledge). Cancel safe: a request dropped while it waits gives its
/// place, or what it was granted, to the next in line.
pub(crate) async fn acquire(
    pool: &Shared,
    max: usize,
    idle_timeout: Duration,
    fresh: bool,
    h2: bool,
) -> Taken {
    let waiter = {
        let mut p = pool.borrow_mut();
        p.h2_known |= h2;
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
            if p.h2_known && p.h2_opening {
                // Wait for the HTTP/2 connection being opened.
            } else if p.open < max {
                p.open += 1;
                p.h2_opening = p.h2_known;
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
        Grant::Retry => Taken::Retry,
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
        p.h2_opening = p.h2_known;
        drop(p);
        grant(&w, Grant::Open);
        return;
    }
    p.open -= 1;
    p.h2_opening = false;
}

/// The HTTP/2 connection with the most room, if one has room.
pub(crate) fn h2_with_room(pool: &Shared) -> Option<h2::Connection> {
    let p = pool.borrow();
    p.h2.iter()
        .filter(|(_, c)| c.has_room())
        .min_by_key(|(_, c)| c.streams())
        .map(|(_, c)| c.clone())
}

/// An identifier for the next HTTP/2 connection.
pub(crate) fn next_h2_id(pool: &Shared) -> u64 {
    let mut p = pool.borrow_mut();
    p.next_h2 += 1;
    p.next_h2
}

/// A connection just opened speaks HTTP/2: it keeps its place in `open`
/// until it ends ([`h2_closed`]), and everyone in line looks again.
pub(crate) fn add_h2(pool: &Shared, id: u64, conn: h2::Connection) {
    let mut p = pool.borrow_mut();
    p.h2.push((id, conn));
    p.h2_known = true;
    p.h2_opening = false;
    retry_all(p);
}

/// A connection just opened to an `https` origin speaks HTTP/1.1: requests
/// in line no longer wait for an HTTP/2 one.
pub(crate) fn opened_h1(pool: &Shared) {
    let mut p = pool.borrow_mut();
    if p.h2_known {
        p.h2_known = false;
        p.h2_opening = false;
        retry_all(p);
    }
}

fn retry_all(mut p: std::cell::RefMut<'_, HostPool>) {
    let mut waiters = Vec::new();
    while let Some(w) = p.next_waiter() {
        waiters.push(w);
    }
    drop(p);
    for w in waiters {
        grant(&w, Grant::Retry);
    }
}

/// A stream of an HTTP/2 connection ended: the first in line may use its
/// room.
pub(crate) fn h2_stream_closed(pool: &Shared) {
    let mut p = pool.borrow_mut();
    if p.waiters.is_empty() || !p.h2.iter().any(|(_, c)| c.has_room()) {
        return;
    }
    if let Some(w) = p.next_waiter() {
        drop(p);
        grant(&w, Grant::Retry);
    }
}

/// HTTP/2 connection `id` ended: it leaves the pool, and its place goes to
/// the first in line.
pub(crate) fn h2_closed(pool: &Shared, id: u64) {
    let mut p = pool.borrow_mut();
    let before = p.h2.len();
    p.h2.retain(|(i, _)| *i != id);
    if p.h2.len() == before {
        return;
    }
    drop(p);
    free(pool);
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
            // Passed on, in case it was the room a stream left.
            Some(Grant::Retry) => h2_stream_closed(self.pool),
            None => self
                .pool
                .borrow_mut()
                .waiters
                .retain(|w| !Rc::ptr_eq(w, &self.waiter)),
        }
    }
}
