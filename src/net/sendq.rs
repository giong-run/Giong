//! Datagrams sent in one turn of the event loop go to the kernel together.
//!
//! A task that sends a datagram while other tasks are waiting to run (or
//! while datagrams of the same socket are already waiting) puts it in the
//! worker's queue and waits. When the tasks of the turn have run, the loop
//! hands the queue to the kernel with one `sendmmsg(2)` per run of datagrams
//! on the same socket, and wakes each sender with its own result. The result
//! is the same as from `sendto(2)`: bytes sent, or the error for that
//! datagram; a datagram the socket had no room for goes back to its sender,
//! which then waits for room as before.
//!
//! A task that is the only one with work sends at once, so a lone sender pays
//! no extra turn. A queued datagram waits at most until the end of the next
//! turn (see [`SendQueue::due`]), the same bound as an io_uring submission.
//!
//! Datagrams of one socket leave in the order they were sent: once one is
//! queued, later ones on that socket queue behind it.

use std::future::poll_fn;
use std::io;
#[cfg(unix)]
use std::ptr;
use std::task::{Poll, Waker};

use socket2::SockAddr;

use crate::runtime;
use crate::slab::Slab;
use crate::sys::RawSock as RawFd;

/// Datagrams handed to the kernel per `sendmmsg` (Linux takes up to 1024).
const CHUNK: usize = 64;

/// No socket: what a datagram that went away is grouped under.
#[cfg(unix)]
const NO_SOCKET: RawFd = -1;
#[cfg(windows)]
const NO_SOCKET: RawFd = RawFd::MAX;

/// What became of a queued datagram.
enum State {
    /// Waiting for the end of the turn; the waker of its sender.
    Queued(Waker),
    /// `sendmmsg`'s answer: bytes sent, or a negative errno.
    Done(isize),
    /// The socket had no room: the sender takes the slower path.
    Full,
}

struct Entry {
    fd: RawFd,
    buf: *const u8,
    len: usize,
    /// `None` for a connected socket.
    addr: Option<SockAddr>,
    state: State,
}

/// The worker's queue of datagrams for the end of the turn.
pub(crate) struct SendQueue {
    entries: Slab<Entry>,
    /// Keys of queued entries, oldest first.
    order: Vec<usize>,
    /// The loop turn in which the oldest queued entry was queued: `None`
    /// until the end of that turn names it (see [`due`](Self::due)).
    since: Option<u32>,
}

impl SendQueue {
    pub(crate) fn new() -> Self {
        SendQueue {
            entries: Slab::new(),
            order: Vec::new(),
            since: None,
        }
    }

    /// `true` if the queue should go to the kernel after the turn `tick`:
    /// no other task is runnable (`idle`), or its oldest datagram was queued
    /// in an earlier turn.
    pub(crate) fn due(&mut self, tick: u32, idle: bool) -> bool {
        if self.order.is_empty() {
            return false;
        }
        let since = *self.since.get_or_insert(tick);
        idle || since != tick || self.order.len() >= CHUNK * 4
    }

    fn holds(&self, fd: RawFd) -> bool {
        self.order
            .iter()
            .any(|&k| self.entries.get(k).is_some_and(|e| e.fd == fd))
    }

    /// Sends what is queued and returns the wakers of the senders.
    pub(crate) fn flush(&mut self) -> Vec<Waker> {
        let order = std::mem::take(&mut self.order);
        let mut wakers = Vec::with_capacity(order.len());
        let mut i = 0;
        while i < order.len() {
            let fd = self.entries.get(order[i]).map_or(NO_SOCKET, |e| e.fd);
            let mut j = i;
            while j < order.len()
                && j - i < CHUNK
                && self.entries.get(order[j]).map_or(NO_SOCKET, |e| e.fd) == fd
            {
                j += 1;
            }
            self.send_run(&order[i..j], &mut wakers);
            i = j;
        }
        // Keep the allocation.
        let mut order = order;
        order.clear();
        self.order = order;
        wakers
    }

    /// One `sendmmsg` (more after an error) for `keys`, all on one socket.
    #[cfg(unix)]
    fn send_run(&mut self, keys: &[usize], wakers: &mut Vec<Waker>) {
        let mut iov: Vec<libc::iovec> = Vec::with_capacity(keys.len());
        let mut msgs: Vec<crate::sys::MMsgHdr> = Vec::with_capacity(keys.len());
        let fd = self.entries.get(keys[0]).map_or(NO_SOCKET, |e| e.fd);
        for &k in keys {
            let e = self.entries.get(k).expect("queued datagram");
            iov.push(libc::iovec {
                iov_base: e.buf.cast_mut().cast(),
                iov_len: e.len,
            });
        }
        for (n, &k) in keys.iter().enumerate() {
            let e = self.entries.get(k).expect("queued datagram");
            // SAFETY: an all-zero `mmsghdr` is a valid empty header.
            let mut m: crate::sys::MMsgHdr = unsafe { std::mem::zeroed() };
            if let Some(a) = &e.addr {
                m.msg_hdr.msg_name = a.as_ptr().cast_mut().cast();
                m.msg_hdr.msg_namelen = a.len();
            }
            m.msg_hdr.msg_iov = ptr::addr_of_mut!(iov[n]);
            m.msg_hdr.msg_iovlen = 1;
            msgs.push(m);
        }
        let mut at = 0;
        while at < keys.len() {
            let left = (keys.len() - at).min(u32::MAX as usize) as u32;
            // SAFETY: `msgs[at..]` are valid headers pointing at the queued
            // buffers and addresses, which stay put until their senders are
            // woken; non-blocking.
            let res = unsafe {
                crate::sys::sendmmsg(
                    fd,
                    msgs.as_mut_ptr().add(at),
                    left as usize,
                    crate::sys::MSG_NOSIGNAL | crate::sys::MSG_DONTWAIT,
                )
            };
            if res > 0 {
                for (n, &k) in keys[at..at + res as usize].iter().enumerate() {
                    let sent = msgs[at + n].msg_len as isize;
                    self.finish(k, State::Done(sent), wakers);
                }
                at += res as usize;
                continue;
            }
            match io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(crate::sys::errno::EIO)
            {
                crate::sys::errno::EINTR => {}
                crate::sys::errno::EAGAIN => {
                    for &k in &keys[at..] {
                        self.finish(k, State::Full, wakers);
                    }
                    return;
                }
                // The error belongs to the first datagram of the rest: the
                // ones after it get their own attempt.
                e => {
                    self.finish(keys[at], State::Done(-(e as isize)), wakers);
                    at += 1;
                }
            }
        }
    }

    /// One non-blocking send for each of `keys`: Windows has no call that
    /// takes several datagrams. A socket without room ends the run, and the
    /// rest wait for room on their own.
    #[cfg(windows)]
    fn send_run(&mut self, keys: &[usize], wakers: &mut Vec<Waker>) {
        for (at, &k) in keys.iter().enumerate() {
            let e = self.entries.get(k).expect("queued datagram");
            let res = loop {
                // SAFETY: the queued buffer and address stay put until
                // their sender is woken; non-blocking.
                let res = unsafe {
                    match &e.addr {
                        Some(a) => crate::sys::send_to_nowait(
                            e.fd,
                            e.buf,
                            e.len,
                            a.as_ptr().cast(),
                            a.len(),
                        ),
                        None => crate::sys::send_nowait(e.fd, e.buf, e.len),
                    }
                };
                if res >= 0 {
                    break res;
                }
                match io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(crate::sys::errno::EIO)
                {
                    crate::sys::errno::EINTR => {}
                    crate::sys::errno::EAGAIN => {
                        for &k in &keys[at..] {
                            self.finish(k, State::Full, wakers);
                        }
                        return;
                    }
                    e => break -(e as isize),
                }
            };
            self.finish(k, State::Done(res), wakers);
        }
    }

    fn finish(&mut self, key: usize, state: State, wakers: &mut Vec<Waker>) {
        if let Some(e) = self.entries.get_mut(key) {
            if let State::Queued(w) = std::mem::replace(&mut e.state, state) {
                wakers.push(w);
            }
        }
    }
}

/// Removes a queued datagram whose sender went away before the end of the
/// turn, so that the queue never points at a dropped buffer.
struct Guard(Option<usize>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            runtime::try_with_worker(|w| {
                let mut q = w.sends.borrow_mut();
                q.entries.remove(key);
                if let Some(at) = q.order.iter().position(|&k| k == key) {
                    q.order.remove(at);
                }
            });
        }
    }
}

/// `true` if a send now may go to the worker's queue (see [`send`]): other
/// tasks are runnable, or datagrams already wait. Cheap, for the callers'
/// fast path.
#[inline]
pub(crate) fn wanted(w: &runtime::Worker) -> bool {
    w.sends_waiting.get() || !w.queue.borrow().is_empty()
}

/// What [`send`] did.
pub(crate) enum Sent {
    /// Bytes sent, or a negative errno, as from `sendto(2)`.
    Done(isize),
    /// Not queued: the caller sends on its own.
    NotQueued,
    /// The socket had no room: the caller waits for room on its own.
    Full,
}

/// Queues `len` bytes at `buf` for `fd` (to `addr`, or to the connected
/// peer) if other tasks of this turn may send too, and waits for the end of
/// the turn.
///
/// # Safety
///
/// `buf` must stay valid for `len` bytes until the returned future completes
/// or is dropped.
pub(crate) async unsafe fn send(
    fd: RawFd,
    buf: *const u8,
    len: usize,
    addr: Option<&SockAddr>,
) -> Sent {
    let mut guard = Guard(None);
    poll_fn(|cx| {
        runtime::with_worker(|w| {
            let mut q = w.sends.borrow_mut();
            let Some(key) = guard.0 else {
                if w.queue.borrow().is_empty() && !q.holds(fd) {
                    return Poll::Ready(Sent::NotQueued);
                }
                if q.order.is_empty() {
                    q.since = None;
                }
                let key = q.entries.insert(Entry {
                    fd,
                    buf,
                    len,
                    addr: addr.cloned(),
                    state: State::Queued(cx.waker().clone()),
                });
                q.order.push(key);
                w.sends_waiting.set(true);
                guard.0 = Some(key);
                return Poll::Pending;
            };
            let e = q.entries.get_mut(key).expect("queued datagram");
            let sent = match &mut e.state {
                State::Queued(waker) => {
                    if !waker.will_wake(cx.waker()) {
                        *waker = cx.waker().clone();
                    }
                    return Poll::Pending;
                }
                State::Done(res) => Sent::Done(*res),
                State::Full => Sent::Full,
            };
            q.entries.remove(key);
            guard.0 = None;
            Poll::Ready(sent)
        })
    })
    .await
}
