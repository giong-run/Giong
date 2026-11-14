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
    /// Balanced listeners this worker has open on the address. Only that
    /// worker changes it; other workers read it as a hint when they choose
    /// where to send a connection.
    listeners: AtomicUsize,
    inbox: Mutex<Inbox>,
}

#[derive(Default)]
struct Inbox {
    sockets: VecDeque<Socket>,
    /// The task waiting in `accept` on this worker, if any.
    waiter: Option<Waker>,
    /// The worker has a balanced listener on the address, so something will
    /// take what is put here. Changed with `listeners` going from 0 to 1 and
    /// back, under this lock, so that a connection is never put in a mailbox
    /// that its worker has already emptied for the last time.
    open: bool,
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
        let mailbox = &group.mailboxes[worker];
        if mailbox.listeners.fetch_add(1, Ordering::AcqRel) == 0 {
            lock(&mailbox.inbox).open = true;
        }
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
            let sock = match self.busier_than_another() {
                Some(target) => match group.hand_over(target, sock) {
                    Ok(()) => continue,
                    // Its last listener went in the meantime: keep it.
                    Err(sock) => sock,
                },
                None => sock,
            };
            // Counted here at once: several connections can arrive in one
            // batch, before this worker publishes its load.
            group.rt.count_local(self.worker);
            return Poll::Ready(Ok(sock));
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
    /// Puts `sock` in `worker`'s mailbox and wakes its `accept`, or gives it
    /// back if that worker no longer has a balanced listener on the address
    /// (its last one may go between the choice and this call).
    fn hand_over(&self, worker: usize, sock: Socket) -> Result<(), Socket> {
        self.rt.sending(worker);
        let waiter = {
            let mut inbox = lock(&self.mailboxes[worker].inbox);
            if !inbox.open {
                drop(inbox);
                self.rt.received(worker);
                return Err(sock);
            }
            inbox.sockets.push_back(sock);
            inbox.waiter.take()
        };
        if let Some(waker) = waiter {
            waker.wake();
        }
        Ok(())
    }

    /// Hands `sock` to the least busy member other than `except`, or closes
    /// it if there is none.
    fn hand_to_another(&self, except: usize, mut sock: Socket) {
        // Each refusal means a member closed its last listener; there are
        // only so many members.
        for _ in 0..self.mailboxes.len() {
            let target = self.rt.least_loaded_of(None, |i| {
                i != except && self.mailboxes[i].listeners.load(Ordering::Acquire) > 0
            });
            match target {
                Some(target) => match self.hand_over(target, sock) {
                    Ok(()) => return,
                    Err(back) => sock = back,
                },
                None => break,
            }
        }
        drop(sock);
    }
}

impl Drop for Member {
    fn drop(&mut self) {
        let group = &*self.group;
        let mailbox = &group.mailboxes[self.worker];
        if mailbox.listeners.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // The worker's last balanced listener on this address: close the
        // mailbox, so that no other worker puts a connection in it from now
        // on, and send what waits in it to another member, or close it.
        let waiting = {
            let mut inbox = lock(&mailbox.inbox);
            inbox.open = false;
            inbox.waiter = None;
            std::mem::take(&mut inbox.sockets)
        };
        for sock in waiting {
            group.rt.received(self.worker);
            group.hand_to_another(self.worker, sock);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener as StdListener};
    use std::sync::{Arc, Barrier, Mutex};

    use socket2::Socket;

    use super::Member;

    /// A connected socket, for handing over.
    fn connected(listener: &StdListener) -> Socket {
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        Socket::from(stream)
    }

    #[test]
    fn a_mailbox_whose_last_listener_is_gone_refuses_connections() {
        // Worker 0 picks worker 1 while it still has a listener; worker 1
        // drops its last one; then worker 0 hands the connection over. It
        // used to land in a mailbox nobody would read again, and the client
        // got neither a reply nor a close.
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let peer = StdListener::bind("127.0.0.1:0").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let result = Arc::new(Mutex::new(None));
        let rt = crate::Builder::new().workers(2).build().unwrap();
        let (b, r) = (barrier.clone(), result.clone());
        let peer = Arc::new(peer);
        rt.run(move |worker| {
            let (barrier, result, peer) = (b.clone(), r.clone(), peer.clone());
            async move {
                let member = Member::join(addr);
                barrier.wait(); // both members joined
                if worker == 1 {
                    drop(member);
                    barrier.wait(); // worker 1's last listener is gone
                    return;
                }
                barrier.wait();
                let group = member.group.clone();
                let refused = group.hand_over(1, connected(&peer)).is_err();
                let stranded = super::lock(&group.mailboxes[1].inbox).sockets.len();
                *result.lock().unwrap() = Some((refused, stranded));
            }
        })
        .unwrap();
        assert_eq!(*result.lock().unwrap(), Some((true, 0)));
    }
}
