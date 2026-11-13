//! Sharing new connections between the workers' listeners on one address
//! ([`TcpListener::bind_balanced`](super::TcpListener::bind_balanced)).
//!
//! Every balanced listener of a runtime on a given address belongs to one
//! [`Group`]. Each worker has a mailbox in the group. A listener that
//! accepts a connection while another member looks clearly less busy puts
//! the socket in that member's mailbox instead of returning it; the
//! member's `accept` takes it from there. Only the descriptor crosses
//! threads: the receiving worker wraps it in its own `TcpStream`, and the
//! stream's I/O starts on that worker's ring.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use socket2::Socket;

use super::ListenSocket;
use crate::runtime::{self, lock, RuntimeShared};

/// How much busier than the least busy member a worker must look before it
/// hands a connection over. Above zero so that two workers that are about
/// as busy do not pass connections back and forth; the accepting worker is
/// awake, which alone counts one (see `RuntimeShared::score`).
const MARGIN: usize = 2;

/// The balanced listeners of one runtime on one address.
struct Group {
    rt: Arc<RuntimeShared>,
    addr: SocketAddr,
    /// One per worker of the runtime, by worker index.
    mailboxes: Vec<Mailbox>,
}

struct Mailbox {
    /// Balanced listeners this worker has open on the address.
    listeners: AtomicUsize,
    inbox: Mutex<Inbox>,
}

#[derive(Default)]
struct Inbox {
    sockets: VecDeque<Socket>,
    /// The task waiting in `accept` on this worker, if any.
    waiter: Option<Waker>,
}

/// Every group alive in the process. Entries are dropped once their group
/// is gone.
static GROUPS: Mutex<Vec<Weak<Group>>> = Mutex::new(Vec::new());

/// A balanced listener's place in its group.
pub(crate) struct Member {
    group: Arc<Group>,
    worker: usize,
}

impl Member {
    /// Joins (or starts) the group of the current runtime on `addr`.
    pub(crate) fn join(addr: SocketAddr) -> Member {
        let (rt, worker) = runtime::with_worker(|w| (w.rt.clone(), w.index));
        let group = {
            let mut groups = lock(&GROUPS);
            groups.retain(|g| g.strong_count() > 0);
            let found = groups
                .iter()
                .filter_map(Weak::upgrade)
                .find(|g| Arc::ptr_eq(&g.rt, &rt) && g.addr == addr);
            match found {
                Some(group) => group,
                None => {
                    let mailboxes = (0..rt.workers())
                        .map(|_| Mailbox {
                            listeners: AtomicUsize::new(0),
                            inbox: Mutex::new(Inbox::default()),
                        })
                        .collect();
                    let group = Arc::new(Group {
                        rt,
                        addr,
                        mailboxes,
                    });
                    groups.push(Arc::downgrade(&group));
                    group
                }
            }
        };
        group.mailboxes[worker]
            .listeners
            .fetch_add(1, Ordering::AcqRel);
        Member { group, worker }
    }

    /// The next connection for this worker: one handed over by another
    /// member, or one accepted here that no other member should take.
    pub(crate) fn poll_accept(
        &self,
        cx: &mut Context<'_>,
        listener: &ListenSocket,
    ) -> Poll<io::Result<Socket>> {
        let group = &*self.group;
        {
            let mut inbox = lock(&group.mailboxes[self.worker].inbox);
            if let Some(sock) = inbox.sockets.pop_front() {
                group.rt.received(self.worker);
                return Poll::Ready(Ok(sock));
            }
            match &inbox.waiter {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => inbox.waiter = Some(cx.waker().clone()),
            }
        }
        loop {
            let sock = match listener.poll_accept(cx) {
                Poll::Ready(Ok(sock)) => sock,
                other => return other,
            };
            match self.busier_than_another() {
                Some(target) => group.hand_over(target, sock),
                None => {
                    // Counted here at once: several connections can arrive
                    // in one batch, before this worker publishes its load.
                    group.rt.count_local(self.worker);
                    return Poll::Ready(Ok(sock));
                }
            }
        }
    }

    /// Another member that looks clearly less busy than this worker.
    fn busier_than_another(&self) -> Option<usize> {
        let group = &*self.group;
        let rt = &group.rt;
        let target = rt.least_loaded_of(None, |i| {
            i != self.worker && group.mailboxes[i].listeners.load(Ordering::Acquire) > 0
        })?;
        (rt.score(self.worker) > rt.score(target) + MARGIN).then_some(target)
    }
}

impl Group {
    /// Puts `sock` in `worker`'s mailbox and wakes its `accept`.
    fn hand_over(&self, worker: usize, sock: Socket) {
        self.rt.sending(worker);
        let waiter = {
            let mut inbox = lock(&self.mailboxes[worker].inbox);
            inbox.sockets.push_back(sock);
            inbox.waiter.take()
        };
        if let Some(waker) = waiter {
            waker.wake();
        }
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        let group = &*self.group;
        let mailbox = &group.mailboxes[self.worker];
        if mailbox.listeners.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // The worker's last balanced listener on this address: connections
        // still waiting for it go to another member, or are closed.
        let waiting = {
            let mut inbox = lock(&mailbox.inbox);
            inbox.waiter = None;
            std::mem::take(&mut inbox.sockets)
        };
        for sock in waiting {
            group.rt.received(self.worker);
            let target = group.rt.least_loaded_of(None, |i| {
                i != self.worker && group.mailboxes[i].listeners.load(Ordering::Acquire) > 0
            });
            match target {
                Some(target) => group.hand_over(target, sock),
                None => drop(sock),
            }
        }
    }
}
