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
//!
//! Which member looks less busy is decided in two steps. First by how busy
//! each worker has been lately: the CPU time its thread used in the last
//! few milliseconds, less the time it spent spinning for work, as a share
//! of the time that passed, sampled at most every
//! [`SAMPLE_EVERY_NS`] by whichever member accepts a connection (see
//! [`Busy`]), plus the expected cost of the work it took on since. Workers
//! within [`BAND`] of the least busy one count as equally busy; among
//! those, by counts of tasks, as before (see `RuntimeShared::score`). So a
//! worker with thousands of idle connections still gets new ones while a
//! worker with a few hot ones is saturated, and when every worker is about
//! as busy, connection counts decide. When every worker is saturated
//! ([`SATURATED`]), counts of tasks would mislead (the idle connections
//! went to the workers that had room), so the work each one has waiting
//! decides instead.

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use socket2::Socket;

use super::ListenSocket;
use crate::observe::clock_ns;
use crate::runtime::{self, lock, RuntimeShared};

/// How much busier than the least busy member a worker must look before it
/// hands a connection over. Above zero so that two workers that are about
/// as busy do not pass connections back and forth; the accepting worker is
/// awake, which alone counts one (see `RuntimeShared::score`).
const MARGIN: usize = 2;

/// One whole core, in the parts-per-million unit busy shares are kept in.
const CORE: u64 = 1_000_000;

/// How often how busy each worker has been is measured again, at most: a
/// sample costs one system call per worker (reading its CPU time), made by
/// the member that accepts a connection once the last sample is this old.
const SAMPLE_EVERY_NS: u64 = 5_000_000;

/// A sample this much older than the last one replaces it instead of being
/// averaged with it: what the workers did that long ago says little.
const STALE_NS: u64 = 4 * SAMPLE_EVERY_NS;

/// Workers whose expected busy share is within this much of the least busy
/// one count as equally busy (5% of a core). Wide enough that measurement
/// noise does not move connections; narrow enough that a worker busy 40% of
/// the time does not get the same share of new connections as one busy 20%.
const BAND: u64 = CORE / 20;

/// When even the least busy member is expected to be this busy (80% of a
/// core), every member counts as saturated, and new connections go by
/// backlog: runnable tasks and work sent but not taken in yet (see
/// [`BACKLOG_MARGIN`]).
const SATURATED: u64 = CORE * 8 / 10;

/// When every member is saturated, how much more backlog than another
/// member the accepting worker must have before it hands a connection
/// over. Hot connections keep a worker's backlog moving between 0 and a few;
/// a smaller margin hands connections over on that noise and ends up
/// spreading hot connections evenly, which leaves light ones waiting behind
/// as many hot ones wherever they land (`bench/placement.sh`, mixed); a
/// larger one lets a queue of short requests build up on one worker
/// (skewed, back to back).
const BACKLOG_MARGIN: usize = 2;

/// The least each task taken on since the last sample is expected to cost
/// (0.1% of a core), so that a burst of new connections within one sample
/// does not all go to the worker that was least busy when it began, even
/// when every connection measured so far was idle. Kept small, so that
/// counts of tasks still decide among workers that are about as busy: one
/// worker must take on 50 connections more than another within one sample
/// before this alone makes it look clearly busier.
const MIN_COST: u64 = CORE / 1000;

/// The balanced listeners of one runtime on one address.
struct Group {
    rt: Arc<RuntimeShared>,
    addr: SocketAddr,
    /// One per worker of the runtime, by worker index.
    mailboxes: Vec<Mailbox>,
    busy: Busy,
}

/// How busy each worker of the group's runtime has been lately: the CPU
/// time of its thread, which the kernel keeps, less the time it spent
/// spinning for work (see `RuntimeShared::busy_ns_of`). CPU time rather
/// than time awake, so that a worker whose thread waits for a core on a
/// crowded machine does not look busy for it. Nothing is added to the
/// workers' own loops; members take a sample when they accept a connection
/// and the last one is old enough, one at a time, and never wait for each
/// other. All values are hints, read and written with relaxed atomics.
struct Busy {
    /// When the last sample was taken, on [`clock_ns`]; 0 before the first.
    taken_at: AtomicU64,
    /// A member is taking a sample; the others use the last one.
    sampling: AtomicBool,
    /// What each live task cost on average in the last sample, in parts per
    /// million of a core, at least [`MIN_COST`]: the expected cost of a
    /// connection a worker took on since.
    per_task: AtomicU64,
    workers: Vec<WorkerBusy>,
}

/// In `WorkerBusy::busy_at`: the worker's CPU time could not be read at the
/// last sample, so the next one has nothing to compare with.
const NO_BASE: u64 = u64::MAX;

struct WorkerBusy {
    /// The worker's busy nanoseconds at the last sample, or [`NO_BASE`].
    busy_at: AtomicU64,
    /// Its live tasks at the last sample.
    tasks_at: AtomicUsize,
    /// The share of its time it was busy, in parts per million: averaged
    /// over the last samples.
    share: AtomicU64,
}

impl Busy {
    fn new(workers: usize) -> Self {
        Self {
            taken_at: AtomicU64::new(0),
            sampling: AtomicBool::new(false),
            per_task: AtomicU64::new(MIN_COST),
            workers: (0..workers)
                .map(|_| WorkerBusy {
                    busy_at: AtomicU64::new(NO_BASE),
                    tasks_at: AtomicUsize::new(0),
                    share: AtomicU64::new(0),
                })
                .collect(),
        }
    }

    /// Takes a new sample if the last one is at least [`SAMPLE_EVERY_NS`]
    /// old and no other member is taking one.
    fn refresh(&self, rt: &RuntimeShared) {
        let now = clock_ns();
        let last = self.taken_at.load(Ordering::Acquire);
        if last != 0 && now.saturating_sub(last) < SAMPLE_EVERY_NS {
            return;
        }
        if self.sampling.swap(true, Ordering::Acquire) {
            return;
        }
        // Another member may have finished a sample in the meantime.
        let last = self.taken_at.load(Ordering::Acquire);
        if last == 0 || now.saturating_sub(last) >= SAMPLE_EVERY_NS {
            self.sample(rt, now, last);
        }
        self.sampling.store(false, Ordering::Release);
    }

    fn sample(&self, rt: &RuntimeShared, now: u64, last: u64) {
        let window = now.saturating_sub(last);
        // The first sample only sets the starting point.
        let measured = last != 0 && window > 0;
        let fresh = window <= STALE_NS;
        let (mut busy_sum, mut tasks_sum) = (0u64, 0u64);
        for (i, w) in self.workers.iter().enumerate() {
            let (tasks, _, _) = rt.counts_of(i);
            w.tasks_at.store(tasks, Ordering::Relaxed);
            // A worker whose CPU time cannot be read (not started, ended, or
            // Miri) counts as idle: then counts of tasks decide.
            let Some(busy) = rt.busy_ns_of(i) else {
                w.busy_at.store(NO_BASE, Ordering::Relaxed);
                w.share.store(0, Ordering::Relaxed);
                continue;
            };
            let busy_was = w.busy_at.swap(busy, Ordering::Relaxed);
            if !measured || busy_was == NO_BASE {
                continue;
            }
            // Spinning is taken off by wall time, so the figure can go down
            // after a spin during which the thread was descheduled: that
            // counts as no work.
            let worked = busy.saturating_sub(busy_was).min(window);
            let now_share = (u128::from(worked) * u128::from(CORE) / u128::from(window)) as u64;
            let share = if fresh {
                (w.share.load(Ordering::Relaxed) + now_share) / 2
            } else {
                now_share
            };
            w.share.store(share, Ordering::Relaxed);
            busy_sum += share;
            tasks_sum += tasks as u64;
        }
        if measured {
            let per_task = (busy_sum / tasks_sum.max(1)).max(MIN_COST);
            self.per_task.store(per_task, Ordering::Relaxed);
        }
        self.taken_at.store(now, Ordering::Release);
    }

    /// How busy worker `i` is expected to be: its busy share in the last
    /// sample, plus the average cost of a task for each task it took on
    /// since, each task it has to run, and each piece of work sent to it
    /// that it has not taken in yet.
    fn expected(&self, rt: &RuntimeShared, i: usize) -> u64 {
        let w = &self.workers[i];
        let (tasks, queued, incoming) = rt.counts_of(i);
        let new = tasks.saturating_sub(w.tasks_at.load(Ordering::Relaxed));
        let pending = (new + queued + incoming) as u64;
        w.share
            .load(Ordering::Relaxed)
            .saturating_add(pending.saturating_mul(self.per_task.load(Ordering::Relaxed)))
    }
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
                    let busy = Busy::new(rt.workers());
                    // The starting point of the first measurement.
                    busy.refresh(&rt);
                    let group = Arc::new(Group {
                        rt,
                        addr,
                        mailboxes,
                        busy,
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

    /// Another member that looks clearly less busy than this worker: first
    /// by how busy the workers have been lately, then, among those about as
    /// busy as the least busy one, by their counts of tasks.
    fn busier_than_another(&self) -> Option<usize> {
        let group = &*self.group;
        let rt = &group.rt;
        let busy = &group.busy;
        busy.refresh(rt);
        let member =
            |i: usize| i != self.worker && group.mailboxes[i].listeners.load(Ordering::Acquire) > 0;
        let mine = busy.expected(rt, self.worker);
        let least = (0..group.mailboxes.len())
            .filter(|&i| member(i))
            .map(|i| busy.expected(rt, i))
            .min()?
            .min(mine);
        if least >= SATURATED {
            // Every member is busy: how busy says nothing more, and counts
            // of tasks mislead (idle connections sent away from a busy
            // worker earlier make it look emptiest). Go by backlog, the
            // work each one has waiting, and keep the connection unless
            // another member has clearly more room (BACKLOG_MARGIN).
            let backlog = |i: usize| {
                let (_, queued, incoming) = rt.counts_of(i);
                queued + incoming
            };
            let n = group.mailboxes.len();
            let target = (1..n)
                .map(|k| (self.worker + k) % n)
                .filter(|&i| member(i))
                .min_by_key(|&i| backlog(i))?;
            return (backlog(self.worker) > backlog(target) + BACKLOG_MARGIN).then_some(target);
        }
        let limit = least.saturating_add(BAND);
        let target = rt.least_loaded_of(None, |i| member(i) && busy.expected(rt, i) <= limit)?;
        if mine > limit {
            // Clearly busier than another member, whatever the counts.
            return Some(target);
        }
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
