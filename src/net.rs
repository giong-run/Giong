//! TCP, UDP and Unix sockets.
//!
//! * [`TcpListener::accept`] and [`UnixListener::accept`] use a **multishot
//!   accept**: one submission keeps producing connections.
//! * [`TcpStream::recv`], [`UnixStream::recv`] and [`UdpSocket::recv_from`]
//!   use a **multishot receive with provided buffers**: one submission per
//!   socket, and the kernel picks a buffer from the worker's shared pool only
//!   when data actually arrives — idle sockets pin no memory.
//! * Writes take ownership of the buffer and return it with the result. Large
//!   writes on TCP from bytes that cannot change (`&'static [u8]`,
//!   `Rc<[u8]>`, `Arc<[u8]>`) go out **without a copy** where the kernel
//!   allows it ([`IoUringFeature::ZeroCopySend`](crate::IoUringFeature::ZeroCopySend)).
//! * Descriptors are closed through the ring, batched with everything else.
//!
//! That is how it works on io_uring. On the epoll driver the same calls
//! behave the same way, with a non-blocking system call made when the socket
//! is ready (see [`DriverKind`](crate::DriverKind)).

mod addr;
mod balance;
pub(crate) mod dns;
mod sendq;
mod share;
mod udp;
mod unix;

pub use addr::{lookup_host, ToSocketAddrs};
pub use dns::{Resolver, ResolverConfig};
pub(crate) use sendq::SendQueue;
pub use udp::{Datagram, UdpSocket};
pub use unix::{UnixListener, UnixStream};

use std::cell::{Cell, OnceCell};
use std::future::poll_fn;
use std::io;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::net::{Shutdown, SocketAddr};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::buf::{is_shared, sharing, IoBuf, IoBufMut, RecvBuf, Sharing};
use crate::driver::{Driver, Io, MultiIo, MultiKind, MultiPoll, ZeroCopyHold};
use crate::error::{raise, Code};
use crate::kbox::KernelBox;
use crate::op::Op;
use crate::runtime::{self, Worker};

/// The most buffers one `sendmsg` takes (`IOV_MAX` on Linux); a vectored
/// write with more sends them in several calls.
const IOV_MAX: usize = 1024;
/// The most one `sendfile` call is asked for (Linux sends at most this much
/// in one call anyway).
const SENDFILE_MAX: u64 = 0x7fff_f000;
/// Bytes `send_file` sends before it lets the worker's other tasks run.
const SENDFILE_YIELD: u64 = 512 * 1024;

pub(crate) fn os_err(res: i32) -> io::Error {
    io::Error::from_raw_os_error(-res)
}

/// A multishot operation slot owned by a socket: the driver key plus the uid
/// of the worker whose driver it belongs to.
pub(crate) struct MultiSlot {
    kind: MultiKind,
    key: Cell<Option<(usize, usize)>>,
}

impl MultiSlot {
    pub(crate) fn new(kind: MultiKind) -> Self {
        Self {
            kind,
            key: Cell::new(None),
        }
    }

    /// The driver key on the current worker, creating the slot on first use.
    pub(crate) fn key(&self, w: &Worker) -> usize {
        match self.key.get() {
            Some((key, uid)) if uid == w.shared.uid => key,
            // First use — or the runtime that owned the old key is gone, in
            // which case its teardown already cancelled the operation.
            _ => {
                let key = w.driver.borrow_mut().new_multi(self.kind);
                self.key.set(Some((key, w.shared.uid)));
                key
            }
        }
    }

    /// `true` if a multishot operation has been set up on the current worker.
    pub(crate) fn is_active(&self, w: &Worker) -> bool {
        matches!(self.key.get(), Some((_, uid)) if uid == w.shared.uid)
    }

    /// Releases the slot: drops its multishot operation now, or at the next
    /// turn if the driver is borrowed.
    fn release(&self, w: &Worker) {
        if let Some((key, uid)) = self.key.take() {
            if uid == w.shared.uid {
                match w.driver.try_borrow_mut() {
                    Ok(mut driver) => driver.drop_multi(key),
                    Err(_) => w.defer_drop_multi(key),
                }
            }
        }
    }
}

/// A pooled receive of less than this is a small message.
const SMALL_RECV: usize = 2048;

/// After this many small messages in a row, a stream in `bulk` mode goes
/// back to the multishot receive: its traffic is small messages again. (One
/// small receive alone proves nothing: the rest of a buffer the kernel has
/// partly filled can be small.)
const SMALL_RECVS_TO_LEAVE_BULK: u8 = 8;

/// What [`StreamSocket::recv_into`] found when it stopped the pooled receive.
enum Stopped {
    Data(RecvBuf),
    Item(i32),
    Empty,
}

/// A task's place among those waiting on one [`MultiSlot`], for a future
/// that polls it. Several tasks may wait on the same datagram socket; each
/// completion wakes one of them. A future
/// dropped while it waits gives its place up, and if it was woken for a
/// completion it did not take, the next waiting task is woken for it, so
/// that no completion is left with nobody woken for it.
pub(crate) struct Waiting<'a> {
    slot: &'a MultiSlot,
    waker: Option<Waker>,
}

impl<'a> Waiting<'a> {
    pub(crate) fn new(slot: &'a MultiSlot) -> Self {
        Self { slot, waker: None }
    }

    /// Records the outcome of one poll of the slot.
    pub(crate) fn polled<T>(&mut self, poll: &Poll<T>, cx: &Context<'_>) {
        match poll {
            Poll::Pending => match &self.waker {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => self.waker = Some(cx.waker().clone()),
            },
            // Served: the driver took the waker off the list.
            Poll::Ready(_) => self.waker = None,
        }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let Some(waker) = self.waker.take() else {
            return;
        };
        runtime::try_with_worker(|w| {
            if let Some((key, uid)) = self.slot.key.get() {
                if uid == w.shared.uid {
                    // Borrowed only while the driver itself drops futures,
                    // which it does not do for these.
                    if let Ok(mut driver) = w.driver.try_borrow_mut() {
                        driver.leave_multi(key, &waker);
                    }
                }
            }
        });
    }
}

/// Closes `sock` through the current worker's ring, or directly if there is none.
pub(crate) fn close_socket(sock: &mut ManuallyDrop<Socket>, slot: &MultiSlot) {
    let fd = sock.as_raw_fd();
    let closed = runtime::try_with_worker(|w| {
        // Cancel the multishot operation first: it holds a reference to the
        // socket, which would otherwise stay open after the close. (If the
        // driver is borrowed, both are deferred, in this order.)
        slot.release(w);
        match w.driver.try_borrow_mut() {
            Ok(mut driver) => driver.close_fd(fd),
            Err(_) => w.defer_close(fd),
        }
    })
    .is_some();
    if !closed {
        // SAFETY: `sock` is dropped exactly once, here.
        unsafe { ManuallyDrop::drop(sock) };
    }
}

// ---------------------------------------------------------------- shared core

/// A listening stream socket: what [`TcpListener`] and [`UnixListener`] share.
pub(crate) struct ListenSocket {
    sock: ManuallyDrop<Socket>,
    accept: MultiSlot,
    _not_send: PhantomData<*const ()>,
}

impl ListenSocket {
    pub(crate) fn new(sock: Socket) -> Self {
        // macOS has no per-send flag against `SIGPIPE`: the socket carries
        // the option, and the connections it accepts inherit it. socket2
        // sets it on the sockets it creates; this covers a listener that
        // came from somewhere else (`from_std`).
        #[cfg(target_os = "macos")]
        drop(sock.set_nosigpipe(true));
        Self {
            sock: ManuallyDrop::new(sock),
            accept: MultiSlot::new(MultiKind::Accept),
            _not_send: PhantomData,
        }
    }

    pub(crate) fn socket(&self) -> &Socket {
        &self.sock
    }

    pub(crate) async fn accept(&self) -> io::Result<StreamSocket> {
        poll_fn(|cx| self.poll_accept(cx).map_ok(StreamSocket::new)).await
    }

    /// The next accepted socket, not yet tied to this worker.
    pub(crate) fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<Socket>> {
        runtime::with_worker(|w| {
            let key = self.accept.key(w);
            let mut driver = w.driver.borrow_mut();
            loop {
                match driver.poll_multi(key, cx.waker()) {
                    MultiPoll::Item(res) if res >= 0 => {
                        // SAFETY: a fresh descriptor returned by accept.
                        return Poll::Ready(Ok(unsafe { Socket::from_raw_fd(res) }));
                    }
                    MultiPoll::Item(res) => return Poll::Ready(Err(os_err(res))),
                    MultiPoll::Data(_) => unreachable!("accept never receives data"),
                    MultiPoll::NeedArm => {
                        let fd = self.sock.as_raw_fd();
                        driver.arm_multi(key, MultiIo::Accept { fd });
                    }
                    MultiPoll::Pending => return Poll::Pending,
                }
            }
        })
    }
}

impl Drop for ListenSocket {
    fn drop(&mut self) {
        close_socket(&mut self.sock, &self.accept);
    }
}

/// A zero-copy send may carry this many bytes from buffers that can change
/// (the head of an HTTP response, say) by copying them aside; with more, it
/// is sent with a copy.
const ZERO_COPY_SCRATCH_MAX: usize = 4096;

/// A connected stream socket: what [`TcpStream`] and [`UnixStream`] share.
pub(crate) struct StreamSocket {
    sock: ManuallyDrop<Socket>,
    recv: MultiSlot,
    /// `O_NONBLOCK` is known to be set: always under epoll, and under
    /// io_uring once `send_file` has set it.
    nonblocking: Cell<bool>,
    /// Whether large sends from bytes that cannot change may go out without
    /// a copy: not on a Unix socket, nor once the kernel refused (kernel
    /// TLS) or copied anyway (loopback).
    zero_copy: Cell<bool>,
    /// Set by the driver when the kernel copied a zero-copy send after all;
    /// made at the first zero-copy send.
    zero_copy_copied: OnceCell<Rc<Cell<bool>>>,
    /// Sends of this socket waiting in the driver (see [`SendInFlight`]).
    sends_in_flight: Cell<u32>,
    /// A send was dropped before its result: it may still go out later, so
    /// a send made directly could overtake it. Never cleared.
    send_abandoned: Cell<bool>,
    /// A Unix socket (see [`end_was_missed`](Self::end_was_missed)).
    unix: Cell<bool>,
    /// Data was taken from the multishot receive since the socket was last
    /// checked for an end of stream the kernel did not report.
    check_end: Cell<bool>,
    /// `recv_into` has been used, and small messages have not come in a row
    /// since: the pooled receive takes one buffer at a time instead of all
    /// the data there is (io_uring).
    /// 0, or 1 plus the number of small messages received in a row since.
    bulk: Cell<u8>,
    _not_send: PhantomData<*const ()>,
}

/// Counts a send that waits in the driver, so that [`StreamSocket::send_now`]
/// never sends ahead of it. Dropped before the send's result (a cancelled
/// write), it marks the socket for good: the kernel may still send those
/// bytes, and only sends through the driver stay behind them.
struct SendInFlight<'a> {
    sock: &'a StreamSocket,
    done: bool,
}

impl<'a> SendInFlight<'a> {
    fn new(sock: &'a StreamSocket) -> Self {
        sock.sends_in_flight.set(sock.sends_in_flight.get() + 1);
        Self { sock, done: false }
    }

    fn finish(mut self) {
        self.done = true;
    }
}

impl Drop for SendInFlight<'_> {
    fn drop(&mut self) {
        self.sock
            .sends_in_flight
            .set(self.sock.sends_in_flight.get() - 1);
        if !self.done {
            self.sock.send_abandoned.set(true);
        }
    }
}

impl StreamSocket {
    pub(crate) fn new(sock: Socket) -> Self {
        // macOS waits in a send to a full socket even with `MSG_DONTWAIT`
        // (the flag only keeps a receive from waiting there), so a stream is
        // non-blocking from the start: a send made outside the driver, as
        // giong-compat's direct sends are, must never hold the worker. The
        // driver would set the mode at the first operation anyway.
        #[cfg(target_os = "macos")]
        let nonblocking = sock.set_nonblocking(true).is_ok();
        #[cfg(target_os = "linux")]
        let nonblocking = false;
        Self {
            sock: ManuallyDrop::new(sock),
            recv: MultiSlot::new(MultiKind::Recv),
            nonblocking: Cell::new(nonblocking),
            zero_copy: Cell::new(true),
            zero_copy_copied: OnceCell::new(),
            sends_in_flight: Cell::new(0),
            send_abandoned: Cell::new(false),
            unix: Cell::new(false),
            check_end: Cell::new(false),
            bulk: Cell::new(0),
            _not_send: PhantomData,
        }
    }

    /// A Unix socket: the kernel cannot send from it without a copy, and its
    /// multishot receive can miss the end of the stream (see
    /// [`end_was_missed`](Self::end_was_missed)).
    pub(crate) fn unix(self) -> Self {
        self.zero_copy.set(false);
        self.unix.set(true);
        self
    }

    /// Whether the peer of a Unix socket has stopped sending and the kernel's
    /// multishot receive may have left that unreported. Asked once each time
    /// the reader has taken everything that was received.
    ///
    /// On newer kernels (seen on 6.18.44 and 6.18.63, not on 6.12.117) a
    /// multishot receive that has emptied a Unix socket goes back to waiting
    /// without trying once more. When the data arrived together with the
    /// peer's `shutdown(Write)` and was taken in a pass of its own (more than
    /// 32 buffers were waiting, or the receive was being set up), nothing
    /// wakes the receive again: the end of the stream is never reported, and
    /// a reader that waits for it hangs until the peer closes. A bare ring
    /// without giong shows the same; TCP sockets do not.
    ///
    /// A shutdown that the kernel missed came before the last data was
    /// taken, so it shows here; one that comes later wakes the receive as
    /// usual. Starting the receive again makes the kernel read once more,
    /// which finds the end.
    fn end_was_missed(&self) -> bool {
        if !self.unix.get() || !self.check_end.replace(false) {
            return false;
        }
        let mut fd = libc::pollfd {
            fd: self.sock.as_raw_fd(),
            events: crate::sys::POLL_PEER_CLOSED,
            revents: 0,
        };
        // SAFETY: one valid `pollfd`, and a timeout of zero: this only reads
        // the socket's state.
        let ready = unsafe { libc::poll(&mut fd, 1, 0) };
        ready > 0
            && fd.revents & (crate::sys::POLL_PEER_CLOSED | libc::POLLHUP | libc::POLLERR) != 0
    }

    /// Where a send of `len` bytes from buffers that cannot change should go
    /// out without a copy: the flag the driver sets if the kernel copied it
    /// after all. `None` sends with a copy.
    ///
    /// A zero-copy send has its result before the kernel stops reading the
    /// buffers (once the peer has acknowledged the data), so only buffers
    /// that nobody can change while the driver keeps them ([`sharing`])
    /// qualify: the write then returns at once, and a writer that sends piece
    /// after piece is not held to one piece per round trip.
    fn zero_copy_report(&self, len: usize) -> Option<Rc<Cell<bool>>> {
        if !self.zero_copy.get()
            || !runtime::with_worker(|w| w.driver.borrow().zero_copy_min())
                .is_some_and(|min| len >= min)
        {
            return None;
        }
        let copied = self.zero_copy_copied.get_or_init(Rc::default);
        if copied.get() {
            self.zero_copy.set(false);
            return None;
        }
        Some(copied.clone())
    }

    /// Connects `sock` to `addr` through the driver.
    pub(crate) async fn connect(sock: Socket, addr: SockAddr) -> io::Result<Self> {
        // On the heap so the address the operation points at does not move
        // with the future.
        let addr = KernelBox::new(addr);
        let io = Io::Connect {
            fd: sock.as_raw_fd(),
            addr: addr.get().as_ptr() as *const libc::sockaddr,
            len: addr.get().len(),
        };
        // Owned as a stream from here on, so that a connect that fails, or
        // whose future is dropped, still closes the socket through the
        // driver. The epoll driver registered the descriptor for the
        // attempt; closed behind its back, the number would stay marked as
        // registered, and the next socket to get it would never be woken.
        let stream = Self::new(sock);
        // SAFETY: the operation's only pointer is to the address in its
        // `KernelBox`.
        let (res, _, _addr) = unsafe { Op::new(io, addr) }.await;
        if res < 0 {
            return Err(os_err(res));
        }
        Ok(stream)
    }

    pub(crate) fn socket(&self) -> &Socket {
        &self.sock
    }

    pub(crate) async fn recv(&self) -> io::Result<RecvBuf> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    pub(crate) fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<io::Result<RecvBuf>> {
        runtime::with_worker(|w| {
            let key = self.recv.key(w);
            let mut driver = w.driver.borrow_mut();
            loop {
                match driver.poll_multi(key, cx.waker()) {
                    MultiPoll::Item(res) => {
                        return Poll::Ready(match res {
                            0 => Ok(RecvBuf::eof()),
                            n if n < 0 => Err(os_err(n)),
                            _ => Err(unexpected("io_uring returned data without a buffer")),
                        });
                    }
                    MultiPoll::Data(buf) => {
                        self.check_end.set(true);
                        let bulk = self.bulk.get();
                        if bulk > 0 {
                            self.bulk.set(match buf.len() < SMALL_RECV {
                                // Small messages again: back to the multishot
                                // receive, which costs less for them.
                                true if bulk > SMALL_RECVS_TO_LEAVE_BULK => 0,
                                true => bulk + 1,
                                false => 1,
                            });
                        }
                        return Poll::Ready(Ok(buf));
                    }
                    MultiPoll::NeedArm => {
                        if !wait_for_pool(&mut driver, cx, false) {
                            return Poll::Pending;
                        }
                        let fd = self.sock.as_raw_fd();
                        driver.arm_multi(
                            key,
                            MultiIo::Recv {
                                fd,
                                once: self.bulk.get() > 0,
                            },
                        );
                    }
                    MultiPoll::Pending => {
                        if driver.kind() == crate::DriverKind::IoUring && self.end_was_missed() {
                            driver.restart_multi(key);
                        }
                        return Poll::Pending;
                    }
                }
            }
        })
    }

    pub(crate) async fn read<B: IoBufMut>(&self, mut buf: B) -> (io::Result<usize>, B) {
        if runtime::with_worker(|w| self.recv.is_active(w)) {
            let err = raise(
                Code::MixedReads,
                "read cannot be used after recv on the same stream",
                "use either recv (buffers from the runtime's pool) or read (your own buffer) on \
                 one stream, not both.",
            );
            return (Err(err), buf);
        }
        let len = buf.capacity().min(u32::MAX as usize) as u32;
        let io = Io::Recv {
            fd: self.sock.as_raw_fd(),
            buf: buf.as_mut_ptr(),
            len,
        };
        // SAFETY: an `IoBufMut`'s bytes stay put when it moves.
        let (res, _, mut buf) = unsafe { Op::new(io, buf) }.await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
        // SAFETY: the kernel wrote `res` bytes at the start of the buffer.
        unsafe { buf.set_init(res as usize) };
        (Ok(res as usize), buf)
    }

    /// Receives into the free capacity at the end of `buf` (8 KiB is made
    /// free if there is none), after whatever [`recv`](Self::recv) has taken
    /// from the socket already. Unlike `read`, it may follow `recv` on the
    /// same stream, and `recv` may follow it: the kernel's receive into the
    /// pool is stopped first, and the next `recv` starts it again. For a
    /// large body whose size is known, this saves copying it out of pool
    /// buffers. `Ok(0)` is the end of the stream. Until the pooled receive
    /// brings several small messages in a row, the stream's pooled receives
    /// take one buffer at a time.
    ///
    /// Not cancel safe, like `read`: if the future is dropped, bytes the
    /// receive had already taken from the socket are lost.
    pub(crate) async fn recv_into(&self, mut buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        // From now on the pooled receive takes one buffer at a time, so the
        // next large body is still in the socket when its head is parsed.
        self.bulk.set(1);
        if runtime::with_worker(|w| self.recv.is_active(w)) {
            match poll_fn(|cx| self.poll_stop_recv(cx)).await {
                Stopped::Data(chunk) => {
                    buf.extend_from_slice(&chunk);
                    return (Ok(chunk.len()), buf);
                }
                Stopped::Item(0) => return (Ok(0), buf),
                Stopped::Item(res) if res < 0 => return (Err(os_err(res)), buf),
                Stopped::Item(_) => {
                    return (
                        Err(unexpected("io_uring returned data without a buffer")),
                        buf,
                    )
                }
                Stopped::Empty => {}
            }
        }
        if buf.capacity() == buf.len() {
            buf.reserve(8192);
        }
        let start = buf.len();
        let len = (buf.capacity() - start).min(u32::MAX as usize) as u32;
        let io = Io::Recv {
            fd: self.sock.as_raw_fd(),
            // SAFETY: `start` is within the allocation (at most its capacity).
            buf: unsafe { buf.as_mut_ptr().add(start) },
            len,
        };
        // SAFETY: a `Vec`'s bytes stay put when it moves, and nothing else
        // touches them until the operation hands the vector back.
        let (res, _, mut buf) = unsafe { Op::new(io, buf) }.await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
        // SAFETY: the kernel wrote `res` bytes after the first `start`.
        unsafe { buf.set_len(start + res as usize) };
        (Ok(res as usize), buf)
    }

    /// For [`recv_into`](Self::recv_into): what the pooled receive holds, in
    /// order, then `Empty` once it is stopped and holds nothing. The epoll
    /// driver receives only when asked, so it never holds anything.
    fn poll_stop_recv(&self, cx: &mut Context<'_>) -> Poll<Stopped> {
        runtime::with_worker(|w| {
            let key = self.recv.key(w);
            let mut driver = w.driver.borrow_mut();
            if driver.kind() != crate::DriverKind::IoUring {
                return Poll::Ready(Stopped::Empty);
            }
            match driver.poll_multi(key, cx.waker()) {
                MultiPoll::Item(res) => Poll::Ready(Stopped::Item(res)),
                MultiPoll::Data(buf) => Poll::Ready(Stopped::Data(buf)),
                MultiPoll::NeedArm => Poll::Ready(Stopped::Empty),
                MultiPoll::Pending => {
                    // Its last completion wakes this task.
                    driver.restart_multi(key);
                    Poll::Pending
                }
            }
        })
    }

    pub(crate) async fn write<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        self.send_from(buf, 0).await
    }

    pub(crate) async fn write_all<B: IoBuf>(&self, mut buf: B) -> (io::Result<()>, B) {
        let mut written = 0;
        while written < buf.len() {
            let (res, b) = self.send_from(buf, written).await;
            buf = b;
            match res {
                Ok(0) => return (Err(io::ErrorKind::WriteZero.into()), buf),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return (Err(e), buf),
            }
        }
        (Ok(()), buf)
    }

    pub(crate) async fn write_vectored<B: IoBuf>(
        &self,
        bufs: Vec<B>,
    ) -> (io::Result<usize>, Vec<B>) {
        self.send_vectored_from(bufs, 0).await
    }

    pub(crate) async fn write_all_vectored<B: IoBuf>(
        &self,
        mut bufs: Vec<B>,
    ) -> (io::Result<()>, Vec<B>) {
        let total: usize = bufs.iter().map(IoBuf::len).sum();
        let mut written = 0;
        while written < total {
            let (res, b) = self.send_vectored_from(bufs, written).await;
            bufs = b;
            match res {
                Ok(0) => return (Err(io::ErrorKind::WriteZero.into()), bufs),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return (Err(e), bufs),
            }
        }
        (Ok(()), bufs)
    }

    /// One `sendmsg` of the bytes of `bufs`, taken as one sequence, from
    /// `offset` on.
    async fn send_vectored_from<B: IoBuf>(
        &self,
        bufs: Vec<B>,
        offset: usize,
    ) -> (io::Result<usize>, Vec<B>) {
        let mut iov: Vec<libc::iovec> = pieces_from(&bufs, offset)
            .map(|(buf, skip)| libc::iovec {
                // SAFETY: `skip < len`, so the pointer stays within the
                // buffer.
                iov_base: unsafe { buf.as_ptr().add(skip) }.cast_mut().cast(),
                iov_len: buf.len() - skip,
            })
            .collect();
        if iov.is_empty() {
            return (Ok(0), bufs);
        }
        let hold = self.vectored_hold(&bufs, offset, &mut iov);
        // SAFETY: all-zero is a valid `msghdr` (null pointers, zero
        // lengths).
        let msg: KernelBox<libc::msghdr> = KernelBox::new(unsafe { std::mem::zeroed() });
        // SAFETY: `msg` owns a live `msghdr` nothing else uses yet. The
        // iovec array is a `Vec`'s heap buffer, which moving the `Vec` does
        // not move or reborrow.
        unsafe {
            (*msg.as_ptr()).msg_iov = iov.as_mut_ptr();
            (*msg.as_ptr()).msg_iovlen = iov.len() as _;
        }
        let io = Io::SendMsg {
            fd: self.sock.as_raw_fd(),
            msg: msg.as_ptr(),
            flags: crate::sys::MSG_NOSIGNAL as u32,
            zero_copy: hold.is_some(),
        };
        let in_flight = SendInFlight::new(self);
        // The header, the iovec array (on the heap) and the buffers all stay
        // where they are while the kernel reads them, even if this future is
        // dropped: `Op` keeps them until the operation completes, and the
        // driver keeps the hold of a zero-copy send until the kernel lets go
        // of its bytes. A zero-copy send the socket refuses is sent again
        // with a copy by `Op` itself, so that this future, which every
        // vectored write awaits, keeps nothing else across the send.
        //
        // SAFETY: the header is in a `KernelBox`, the iovec array is a
        // `Vec`'s heap buffer, and the bytes are `IoBuf`s: none of them
        // moves with the tuple. The hold owns what a zero-copy send reads
        // after its result.
        let (res, _, (bufs, _, _)) = unsafe { Op::with_hold(io, (bufs, iov, msg), hold) }.await;
        in_flight.finish();
        if res < 0 {
            (Err(os_err(res)), bufs)
        } else {
            (Ok(res as usize), bufs)
        }
    }

    /// For a send of `iov`, the pieces of `bufs` from `offset` on: what a
    /// zero-copy send of it leaves with the driver, with the pieces from
    /// buffers that can change copied aside and `iov` pointed at the copy.
    /// `None` sends with a copy.
    fn vectored_hold<B: IoBuf>(
        &self,
        bufs: &[B],
        offset: usize,
        iov: &mut [libc::iovec],
    ) -> Option<Box<ZeroCopyHold>> {
        let (mut shared, mut changing) = (0, 0);
        for ((buf, _), v) in pieces_from(bufs, offset).zip(iov.iter()) {
            if is_shared(buf) {
                shared += v.iov_len;
            } else {
                changing += v.iov_len;
            }
        }
        if changing > ZERO_COPY_SCRATCH_MAX {
            return None;
        }
        let copied = self.zero_copy_report(shared)?;
        let mut owners = Vec::new();
        let mut scratch = Vec::with_capacity(changing);
        for ((buf, _), v) in pieces_from(bufs, offset).zip(iov.iter()) {
            match sharing(buf) {
                Sharing::Owner(owner) => owners.push(owner),
                Sharing::Static => {}
                Sharing::No => {
                    // SAFETY: the iovec covers initialised bytes of `buf`.
                    let bytes =
                        unsafe { std::slice::from_raw_parts(v.iov_base as *const u8, v.iov_len) };
                    scratch.extend_from_slice(bytes);
                }
            }
        }
        // `scratch` is complete: its bytes stay where they are from here on,
        // also when the `Vec` moves into the hold.
        let mut at = 0;
        for ((buf, _), v) in pieces_from(bufs, offset).zip(iov.iter_mut()) {
            if !is_shared(buf) {
                v.iov_base = scratch[at..].as_ptr().cast_mut().cast();
                at += v.iov_len;
            }
        }
        Some(Box::new(ZeroCopyHold {
            owners,
            scratch,
            copied,
        }))
    }

    /// One `sendmsg` of `buf` with the ancillary data in `control`.
    pub(crate) async fn send_with_control<B: IoBuf, C: IoBuf>(
        &self,
        buf: B,
        control: C,
    ) -> (io::Result<usize>, B, C) {
        let iov = KernelBox::new(libc::iovec {
            iov_base: buf.as_ptr().cast_mut().cast(),
            iov_len: buf.len(),
        });
        // SAFETY: all-zero is a valid `msghdr` (null pointers, zero lengths).
        let msg: KernelBox<libc::msghdr> = KernelBox::new(unsafe { std::mem::zeroed() });
        // SAFETY: `msg` owns a live `msghdr` nothing else uses yet.
        unsafe {
            let m = msg.as_ptr();
            (*m).msg_iov = iov.as_ptr();
            (*m).msg_iovlen = 1;
            if !control.is_empty() {
                (*m).msg_control = control.as_ptr().cast_mut().cast();
                (*m).msg_controllen = control.len() as _;
            }
        }
        // Control messages go to kernel TLS, which cannot send without a
        // copy.
        let io = Io::SendMsg {
            fd: self.sock.as_raw_fd(),
            msg: msg.as_ptr(),
            flags: crate::sys::MSG_NOSIGNAL as u32,
            zero_copy: false,
        };
        let in_flight = SendInFlight::new(self);
        // The header, the iovec and both buffers stay where they are until the
        // kernel is done with them, even if this future is dropped: `Op`
        // keeps them until the operation completes.
        // SAFETY: the iovec and the header are in `KernelBox`es, and both
        // buffers are `IoBuf`s, whose bytes stay put when they move.
        let (res, _, (buf, control, _iov, _msg)) =
            unsafe { Op::new(io, (buf, control, iov, msg)) }.await;
        in_flight.finish();
        if res < 0 {
            (Err(os_err(res)), buf, control)
        } else {
            (Ok(res as usize), buf, control)
        }
    }

    /// Sends `len` bytes of the file `file` from `offset` with
    /// `sendfile(2)`. See [`TcpStream::send_file`].
    pub(crate) async fn send_file(&self, file: RawFd, offset: u64, len: u64) -> io::Result<()> {
        if offset
            .checked_add(len)
            .map_or(true, |end| end > i64::MAX as u64)
        {
            // No file reaches that far.
            return Err(raise(
                Code::FileTooShort,
                format!(
                    "send_file was asked for {len} bytes from offset {offset}, past any file's end"
                ),
                "pass an offset and a length within the file: offset + len must not exceed \
                 File::len.",
            ));
        }
        self.set_nonblocking()?;
        let fd = self.sock.as_raw_fd();
        let (mut offset, mut left) = (offset, len);
        let mut since_yield = 0;
        while left > 0 {
            let count = left.min(SENDFILE_MAX) as usize;
            // The socket is non-blocking, so the call never waits; `offset`
            // moves past what was sent.
            let mut moved = offset;
            let n = crate::sys::sendfile(fd, file, &mut moved, count);
            match n {
                n if n > 0 => {
                    let n = n as u64;
                    offset += n;
                    left -= n;
                    since_yield += n;
                    if left > 0 && since_yield >= SENDFILE_YIELD {
                        // A fast peer never makes this wait: let the other
                        // tasks of the worker run in between.
                        since_yield = 0;
                        crate::yield_now().await;
                    }
                }
                0 => {
                    return Err(raise(
                        Code::FileTooShort,
                        format!(
                            "the file ended at byte {offset}, {left} bytes short of the {len} \
                             send_file was asked to send"
                        ),
                        "take the length from the open file (File::len), and replace a file \
                         that is being served by renaming a new one over it, not by \
                         rewriting it; the peer has fewer bytes than announced, so close the \
                         connection.",
                    ));
                }
                _ => {
                    let err = io::Error::last_os_error();
                    match err.raw_os_error() {
                        Some(crate::sys::errno::EINTR) => {}
                        Some(crate::sys::errno::EAGAIN) => {
                            // SAFETY: the operation has no pointers.
                            let (res, _, ()) = unsafe { Op::new(Io::Writable { fd }, ()) }.await;
                            if res < 0 {
                                return Err(os_err(res));
                            }
                        }
                        _ => return Err(err),
                    }
                }
            }
        }
        Ok(())
    }

    /// Sets `O_NONBLOCK`, once. io_uring does not need it for its own
    /// operations, and stays correct with it (see [`Op`]).
    fn set_nonblocking(&self) -> io::Result<()> {
        if !self.nonblocking.get() {
            self.sock.set_nonblocking(true)?;
            self.nonblocking.set(true);
        }
        Ok(())
    }

    async fn send_from<B: IoBuf>(&self, buf: B, offset: usize) -> (io::Result<usize>, B) {
        debug_assert!(offset <= buf.len());
        let len = (buf.len() - offset).min(u32::MAX as usize) as u32;
        let hold = self.send_hold(&buf, len as usize);
        // SAFETY: `offset <= len`, so the pointer stays within the buffer.
        let ptr = unsafe { buf.as_ptr().add(offset) };
        if hold.is_none() {
            // SAFETY: `ptr` points at `len` initialised bytes of `buf`.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
            if let Some(res) = self.send_now(bytes) {
                return if res < 0 {
                    (Err(os_err(res)), buf)
                } else {
                    (Ok(res as usize), buf)
                };
            }
        }
        let io = Io::Send {
            fd: self.sock.as_raw_fd(),
            buf: ptr,
            len,
            flags: crate::sys::MSG_NOSIGNAL,
            zero_copy: hold.is_some(),
        };
        let in_flight = SendInFlight::new(self);
        // The hold moves into the send, and a zero-copy send the socket
        // refuses is sent again with a copy by `Op` itself, so that this
        // future, which every write awaits, is the size of a plain send's.
        //
        // SAFETY: `ptr` points into `buf`, an `IoBuf`, whose bytes stay put
        // when it moves. The hold owns what a zero-copy send reads after its
        // result.
        let (res, _, buf) = unsafe { Op::with_hold(io, buf, hold) }.await;
        in_flight.finish();
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Sends at once, without a turn of the event loop, when this task is the
    /// only one that can run: the socket's buffer usually has room, and the
    /// task would otherwise wait for the send's result alone (a request and
    /// its response then cost one turn each way instead of two). While other
    /// tasks are runnable, sends go through the driver instead, where those
    /// of the whole turn reach the kernel together. `None` when the driver
    /// must take it: other tasks to run, a send of this socket still in the
    /// driver, operations queued for the kernel that it would overtake, a
    /// full socket, or the budget spent (see `Driver::try_send`).
    fn send_now(&self, bytes: &[u8]) -> Option<i32> {
        if bytes.is_empty() || self.sends_in_flight.get() != 0 || self.send_abandoned.get() {
            return None;
        }
        runtime::with_worker(|w| {
            if !w.queue.borrow().is_empty() {
                return None;
            }
            let mut driver = w.driver.try_borrow_mut().ok()?;
            // The epoll driver already sends within the operation's first poll.
            // `try_send` declines while operations wait for the kernel: a send
            // made directly would overtake them, cancellations included.
            if driver.kind() != crate::DriverKind::IoUring {
                return None;
            }
            driver.try_send(self.sock.as_raw_fd(), bytes)
        })
    }

    /// What a zero-copy send of `len` bytes of `buf` leaves with the driver,
    /// or `None` for a copying send: bytes that can change, a send too small,
    /// or a socket that cannot send without a copy.
    fn send_hold<B: IoBuf>(&self, buf: &B, len: usize) -> Option<Box<ZeroCopyHold>> {
        if !is_shared(buf) {
            return None;
        }
        let copied = self.zero_copy_report(len)?;
        let owners = match sharing(buf) {
            Sharing::Owner(owner) => vec![owner],
            _ => Vec::new(),
        };
        Some(Box::new(ZeroCopyHold {
            owners,
            scratch: Vec::new(),
            copied,
        }))
    }
}

/// The buffers of `bufs` from byte `offset` on, each with how many of its
/// first bytes to skip; empty ones left out, at most [`IOV_MAX`].
fn pieces_from<B: IoBuf>(bufs: &[B], offset: usize) -> impl Iterator<Item = (&B, usize)> {
    let mut skip = offset;
    bufs.iter()
        .filter_map(move |buf| {
            let len = buf.len();
            if skip >= len {
                skip -= len;
                return None;
            }
            let first = skip;
            skip = 0;
            Some((buf, first))
        })
        .take(IOV_MAX)
}

impl Drop for StreamSocket {
    fn drop(&mut self) {
        close_socket(&mut self.sock, &self.recv);
    }
}

/// Makes sure the kernel has a pool buffer to receive into before a multishot
/// receive (of datagrams, or of a stream) is armed. Returns `false` if the
/// caller has to wait for one (its waker is registered with the pool).
pub(crate) fn wait_for_pool(driver: &mut Driver, cx: &mut Context<'_>, datagram: bool) -> bool {
    if !driver.ensure_recv_buffer(datagram) {
        // The pool is exhausted. Reclaim buffers that hold unread data of
        // other sockets ...
        driver.evacuate();
    }
    if !driver.ensure_recv_buffer(datagram) {
        // ... and if user code holds all the rest, wait for one to come back
        // instead of spinning on ENOBUFS.
        driver.pool().wait(cx.waker());
        return false;
    }
    true
}

// -------------------------------------------------------------------- listener

/// A TCP socket listening for connections.
pub struct TcpListener {
    inner: ListenSocket,
    /// Set by [`bind_balanced`](Self::bind_balanced).
    balance: Option<balance::Member>,
    /// The listener's hold on its port, for one that [`bind`](Self::bind)
    /// made (see `share`).
    held: Option<share::Held>,
}

impl TcpListener {
    /// Listens on `addr`, an IP address and a port.
    ///
    /// Every worker of a process can bind its own listener to the same
    /// address, and connections are then spread across the workers (see
    /// [`bind_balanced`](Self::bind_balanced) for spreading them by load).
    /// No other process can: an address that a socket of another process
    /// holds is refused with [`AddrInUse`](io::ErrorKind::AddrInUse), so a
    /// service started twice fails the second time instead of taking a
    /// share of the first one's connections.
    ///
    /// On Linux the workers share the port through `SO_REUSEPORT`, which by
    /// itself would let any process of the same user bind it too; giong
    /// checks the port before the first listener of a process binds it. To
    /// share a port between processes on purpose (a new version started
    /// before the old one has stopped), create the sockets yourself with
    /// `SO_REUSEPORT` set and pass them to [`from_std`](Self::from_std).
    ///
    /// On macOS the workers share one listening socket instead (each
    /// listener is a duplicate of the first one bound to the address), and
    /// take connections from its one queue as they come: whichever worker
    /// accepts first gets the next connection. Every worker waiting in
    /// `accept` is woken for each connection, and all but one find nothing.
    /// The socket is bound without `SO_REUSEPORT`, so no other user's
    /// process can bind the address while a listener lives. A process of
    /// the same user still can bind one specific IP address of a port that
    /// giong listens on with the wildcard address, by setting
    /// `SO_REUSEADDR` (the BSD rule); listen on the specific address to
    /// refuse that too.
    ///
    /// `addr` is an IP address with a port (see
    /// [`ToSocketAddrs`]): a host name such as
    /// `"localhost:8080"` is refused with `GIONG-E0100`, which says what to
    /// write instead. Port 0 asks the kernel for a free port;
    /// [`local_addr`](Self::local_addr) tells which one it gave.
    ///
    /// ```no_run
    /// # giong::block_on(async {
    /// let listener = giong::net::TcpListener::bind("127.0.0.1:8080")?;
    /// let stream = listener.accept().await?;
    /// # drop(stream);
    /// # Ok::<_, std::io::Error>(())
    /// # }).unwrap();
    /// ```
    pub fn bind<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let mut last_err = None;
        for addr in addr::resolved(&addr, "TcpListener::bind")? {
            match Self::bind_addr(addr) {
                Ok(listener) => return Ok(listener),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(no_addresses))
    }

    fn bind_addr(addr: SocketAddr) -> io::Result<Self> {
        // Linux 6.0.16 to 6.0.19 report success for a bind to an address in
        // use and leave the socket unbound: `share::bind` checks the port.
        let (sock, held) = share::bind(share::Proto::Tcp, addr, true, || {
            Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
        })?;
        let listener = Self {
            inner: ListenSocket::new(sock),
            balance: None,
            held,
        };
        // A failure from here on drops `listener`, which gives the port up.
        listener.inner.socket().listen(4096)?;
        Ok(listener)
    }

    /// Like [`bind`](Self::bind), and the listener shares new connections
    /// with the listeners that other workers of this runtime bound to the
    /// same address with `bind_balanced`.
    ///
    /// The kernel picks a listener for each connection
    /// from a hash of its addresses, without regard to how busy each worker
    /// is: with few connections, or a few heavy ones, one worker can end up
    /// with several while another has none. A balanced listener checks, for
    /// each connection it accepts, whether another worker in its group looks
    /// clearly less busy (by the hints described at
    /// [`Handle::least_loaded_worker`](crate::Handle::least_loaded_worker)),
    /// and if so hands the connection over; that worker's `accept` returns
    /// it. Connections that are already being served never move.
    ///
    /// Call it on every worker, inside [`Runtime::run`](crate::Runtime::run).
    /// A handed-over connection waits for the next `accept` on its new
    /// worker, so keep accepting on every worker; when a balanced listener
    /// is dropped, connections still waiting for it go to another member of
    /// the group, or are closed if it was the last one.
    ///
    /// What it cannot fix: a connection the kernel queued for a worker that
    /// is stuck in a long task waits until that worker accepts again.
    ///
    /// ```no_run
    /// giong::Builder::new().workers(4).build()?.run(|_| async {
    ///     let listener = giong::net::TcpListener::bind_balanced("0.0.0.0:8080").unwrap();
    ///     while let Ok(stream) = listener.accept().await {
    ///         giong::spawn(async move {
    ///             // serve `stream`
    ///             drop(stream);
    ///         });
    ///     }
    /// })?;
    /// # Ok::<(), std::io::Error>(())
    /// ```
    ///
    /// # Panics
    ///
    /// Panics when called outside of a giong runtime.
    pub fn bind_balanced<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let mut listener = Self::bind(addr)?;
        listener.balance = Some(balance::Member::join(listener.local_addr()?));
        Ok(listener)
    }

    /// Wraps a listener created by the standard library, with the options
    /// it was given, and does not check who else holds its port. giong adds
    /// one option on macOS, `SO_NOSIGPIPE`, and the readiness drivers (epoll
    /// and kqueue) make the socket non-blocking when they first use it:
    /// `O_NONBLOCK`, which every descriptor of the socket shares, such as
    /// one from `try_clone`.
    pub fn from_std(listener: std::net::TcpListener) -> Self {
        // SAFETY: we take over the descriptor that `listener` owned.
        let sock = unsafe { Socket::from_raw_fd(listener.into_raw_fd()) };
        Self {
            inner: ListenSocket::new(sock),
            balance: None,
            held: None,
        }
    }

    /// The address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        inet_addr(self.inner.socket().local_addr()?)
    }

    /// Waits for the next incoming connection.
    ///
    /// Cancel safe: dropping the future loses no connection — already accepted
    /// sockets stay queued for the next call.
    ///
    /// Use [`TcpStream::peer_addr`] if you need the remote address. On a
    /// persistent error such as `EMFILE` this returns the error on every call,
    /// so back off (e.g. [`sleep`](crate::time::sleep)) before retrying.
    pub async fn accept(&self) -> io::Result<TcpStream> {
        let inner = match &self.balance {
            None => self.inner.accept().await?,
            Some(member) => {
                poll_fn(|cx| {
                    member
                        .poll_accept(cx, &self.inner)
                        .map_ok(StreamSocket::new)
                })
                .await?
            }
        };
        Ok(TcpStream { inner })
    }
}

impl Drop for TcpListener {
    fn drop(&mut self) {
        // Before the socket is closed, which happens when `inner` is dropped.
        if let Some(held) = self.held.take() {
            held.release(self.inner.socket());
        }
    }
}

impl AsRawFd for TcpListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.socket().as_raw_fd()
    }
}

impl std::fmt::Debug for TcpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpListener")
            .field("fd", &self.as_raw_fd())
            .finish()
    }
}

/// The list of addresses to try was empty.
pub(crate) fn no_addresses() -> io::Error {
    raise(
        Code::NoAddresses,
        "could not resolve to any address",
        "pass at least one address, or a name that resolves to one.",
    )
}

/// The kernel returned something giong does not expect.
pub(crate) fn unexpected(what: &'static str) -> io::Error {
    raise(
        Code::Unexpected,
        what,
        "this is a bug in giong or the kernel; please report it with your kernel version \
         (uname -r) at https://github.com/giong-run/Giong/issues.",
    )
}

pub(crate) fn inet_addr(addr: SockAddr) -> io::Result<SocketAddr> {
    addr.as_socket()
        .ok_or_else(|| io::ErrorKind::AddrNotAvailable.into())
}

// ---------------------------------------------------------------------- stream

/// A TCP connection.
///
/// All methods take `&self`, so a stream can be shared (e.g. in an `Rc`)
/// between one reading task and one writing task. Only one task may read and
/// only one may write at any given time.
pub struct TcpStream {
    inner: StreamSocket,
}

impl TcpStream {
    /// Opens a connection to `addr`: a socket address, or a host name and port
    /// such as `"db.internal:5432"`, resolved without blocking (see
    /// [`lookup_host`]).
    ///
    /// When there are several addresses, they are tried the "Happy Eyeballs"
    /// way (RFC 8305): IPv6 and IPv4 alternate, each attempt gets 250 ms
    /// before the next one starts alongside it, and the first connection made
    /// wins. An address that does not answer therefore costs 250 ms, not a TCP
    /// timeout. Wrap the call in [`timeout`](crate::time::timeout) to bound
    /// the whole thing:
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use giong::net::TcpStream;
    /// # giong::block_on(async {
    /// let stream = giong::time::timeout(Duration::from_secs(3), TcpStream::connect("db.internal:5432"))
    ///     .await
    ///     .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
    /// # Ok::<_, std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// Cancel safe: if the future is dropped, every socket it opened is closed,
    /// so no connection is left behind (the server may have seen one open and
    /// close).
    pub async fn connect<A: ToSocketAddrs>(addr: A) -> io::Result<Self> {
        let addrs = lookup_host(addr).await?;
        addr::connect_first(addrs, Self::connect_addr).await
    }

    /// Connects to exactly this address, without name resolution.
    pub(crate) async fn connect_addr(addr: SocketAddr) -> io::Result<Self> {
        let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        let inner = StreamSocket::connect(sock, addr.into()).await?;
        Ok(Self { inner })
    }

    /// Wraps a connected stream created by the standard library.
    pub fn from_std(stream: std::net::TcpStream) -> Self {
        // SAFETY: we take over the descriptor that `stream` owned.
        let sock = unsafe { Socket::from_raw_fd(stream.into_raw_fd()) };
        // macOS has no per-send flag against `SIGPIPE`: the socket carries
        // the option. socket2 sets it on the sockets it creates; this covers
        // one that came from somewhere else (`from_std`).
        #[cfg(target_os = "macos")]
        drop(sock.set_nosigpipe(true));
        Self {
            inner: StreamSocket::new(sock),
        }
    }

    /// Receives the next chunk of data.
    ///
    /// The returned [`RecvBuf`] borrows a buffer from the worker's pool; drop
    /// it as soon as you are done with the bytes. **An empty buffer means the
    /// peer closed the connection.**
    ///
    /// Cancel safe: dropping the future (for instance through
    /// [`timeout`](crate::time::timeout)) loses no data — whatever arrives in
    /// the meantime is returned by the next call.
    pub async fn recv(&self) -> io::Result<RecvBuf> {
        self.inner.recv().await
    }

    /// The poll form of [`recv`](Self::recv), for futures written by hand
    /// and for adapters to poll-based I/O traits (giong-compat's `Compat`).
    ///
    /// Returns `Poll::Pending` and arranges for `cx`'s waker to be woken when
    /// data or the end of the stream arrives. Only the waker of the most
    /// recent call is woken, so one task at a time should poll a stream.
    /// Nothing is lost by not calling it again: whatever arrives waits for
    /// the next call, as with `recv`.
    ///
    /// Must be called on the worker the stream is used on; it panics outside
    /// a giong runtime.
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<io::Result<RecvBuf>> {
        self.inner.poll_recv(cx)
    }

    /// Reads into `buf` (from its start, up to its capacity) with a one-off
    /// receive, returning the buffer with its length set to the bytes read.
    /// `Ok(0)` means the peer closed the connection, or that `buf` has no
    /// capacity (`Vec::new()`), as in the standard library: give it room
    /// with `Vec::with_capacity`.
    ///
    /// Prefer [`recv`](Self::recv) for servers. The two must not be mixed on
    /// one stream: once `recv` has been used, `read` fails with `InvalidInput`.
    ///
    /// Not cancel safe: if the future is dropped, bytes the receive had
    /// already taken from the socket are lost. [`recv`](Self::recv) is.
    pub async fn read<B: IoBufMut>(&self, buf: B) -> (io::Result<usize>, B) {
        self.inner.read(buf).await
    }

    /// Receives into the free capacity at the end of `buf`, after what
    /// [`recv`](Self::recv) has already taken; `recv` may come before and
    /// after it. For the crates of this workspace (giong-http reads large
    /// request bodies with it), not part of the API.
    ///
    /// Not cancel safe: if the future is dropped, bytes the receive had
    /// already taken from the socket are lost.
    #[doc(hidden)]
    pub async fn recv_into(&self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.inner.recv_into(buf).await
    }

    /// Writes some bytes of `buf`, returning how many were written.
    ///
    /// Not cancel safe: if the future is dropped, some bytes may have been
    /// sent without the caller learning how many.
    pub async fn write<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        self.inner.write(buf).await
    }

    /// Writes all of `buf`. The buffer is handed back so it can be reused.
    ///
    /// 16 KiB and more from bytes that cannot change (`&'static [u8]`,
    /// `Rc<[u8]>`, `Arc<[u8]>`) go out without a copy into the kernel on
    /// Linux 6.2 and newer
    /// ([`IoUringFeature::ZeroCopySend`](crate::IoUringFeature::ZeroCopySend));
    /// other buffers are copied, as is everything on the epoll driver.
    ///
    /// Not cancel safe: if the future is dropped, part of the data may have
    /// been sent.
    pub async fn write_all<B: IoBuf>(&self, buf: B) -> (io::Result<()>, B) {
        self.inner.write_all(buf).await
    }

    /// Writes some bytes of `bufs`, taken as one sequence, with one system
    /// call, returning how many were written.
    ///
    /// Not cancel safe, like [`write`](Self::write).
    pub async fn write_vectored<B: IoBuf>(&self, bufs: Vec<B>) -> (io::Result<usize>, Vec<B>) {
        self.inner.write_vectored(bufs).await
    }

    /// Writes all of `bufs`, one after the other, without copying them into
    /// one buffer first: each `sendmsg` takes up to 1024 of them. The buffers
    /// are handed back so they can be reused.
    ///
    /// Use it to send a header and a large body that live in different
    /// buffers. Small pieces are faster copied into one buffer and sent with
    /// [`write_all`](Self::write_all). A large body from bytes that cannot
    /// change goes out without a copy, as with `write_all`, when the other
    /// buffers hold at most 4 KiB.
    ///
    /// Not cancel safe: if the future is dropped, part of the data may have
    /// been sent.
    pub async fn write_all_vectored<B: IoBuf>(&self, bufs: Vec<B>) -> (io::Result<()>, Vec<B>) {
        self.inner.write_all_vectored(bufs).await
    }

    /// Sends `buf` with one `sendmsg(2)` that carries the ancillary data
    /// (control messages) in `control`, returning how many bytes of `buf`
    /// were sent. Both buffers are handed back.
    ///
    /// `control` holds complete control messages, each a `cmsghdr` followed
    /// by its data and padded the way `CMSG_SPACE` says (an empty `control`
    /// sends none). This is how a protocol layered on the socket tells the
    /// kernel something about one message, such as the record type of a
    /// kernel TLS record (`giong-tls` uses it to send TLS alerts once the
    /// kernel encrypts the connection).
    ///
    /// Not cancel safe: if the future is dropped, the message may have been
    /// sent.
    ///
    /// Hidden from the documentation and outside the semver promise: the
    /// bytes of `control` are Linux's layout, which other systems lay out
    /// differently. It exists for `giong-tls`, and a typed form will replace
    /// it before other systems are supported.
    #[doc(hidden)]
    pub async fn send_with_control<B: IoBuf, C: IoBuf>(
        &self,
        buf: B,
        control: C,
    ) -> (io::Result<usize>, B, C) {
        self.inner.send_with_control(buf, control).await
    }

    /// Sends `len` bytes of `file`, starting at `offset`, straight from the
    /// kernel's page cache with `sendfile(2)`: the bytes are never copied
    /// into the program. The file's own position is neither used nor moved,
    /// so many streams can send from one open file at once.
    ///
    /// On a stream whose TLS records the kernel encrypts (kernel TLS, see
    /// `giong-tls`), the kernel encrypts the file as it sends it; that is
    /// the fastest way to serve a file over TLS.
    ///
    /// Two things to know:
    ///
    /// - The file is read where the call is made: a part of the file that
    ///   is not in the page cache is read from disk while the worker waits,
    ///   as with nginx's `sendfile on`. Files that are served often stay
    ///   cached; for cold files on slow disks, read with
    ///   [`File::read_at`](crate::fs::File::read_at) and write instead.
    /// - `sendfile` cannot ask the kernel not to raise `SIGPIPE` when the
    ///   peer has gone. Rust programs ignore `SIGPIPE` unless they change
    ///   that, and then the call fails with `BrokenPipe`.
    ///
    /// Fails with [`FileTooShort`](crate::error::Code::FileTooShort) if the
    /// file ends before `offset + len`, after sending what it had.
    ///
    /// Not cancel safe: if the future is dropped, part of the file may have
    /// been sent. After the first call, the socket is non-blocking.
    ///
    /// ```no_run
    /// # use giong::net::TcpStream;
    /// # async fn serve(stream: &TcpStream) -> std::io::Result<()> {
    /// let file = giong::fs::File::open("index.html").await?;
    /// let len = file.len()?;
    /// let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {len}\r\n\r\n");
    /// stream.write_all(head.into_bytes()).await.0?;
    /// stream.send_file(&file, 0, len).await
    /// # }
    /// ```
    pub async fn send_file(&self, file: &crate::fs::File, offset: u64, len: u64) -> io::Result<()> {
        self.inner.send_file(file.as_raw_fd(), offset, len).await
    }

    /// Shuts down the read half, the write half, or both.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.socket().shutdown(how)
    }

    /// Enables or disables Nagle's algorithm (`TCP_NODELAY`).
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.inner.socket().set_tcp_nodelay(nodelay)
    }

    /// The remote address of this connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        inet_addr(self.inner.socket().peer_addr()?)
    }

    /// The local address of this connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        inet_addr(self.inner.socket().local_addr()?)
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.socket().as_raw_fd()
    }
}

impl std::fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpStream")
            .field("fd", &self.as_raw_fd())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::share::check_bound;
    use super::*;

    /// What Linux 6.0.19 leaves behind when its bind(2) returns 1 for a busy
    /// address: a socket without a port, which listen(2) would put on a free
    /// one. `check_bound` must call that `AddrInUse`.
    #[test]
    fn a_socket_that_bind_left_unbound_counts_as_address_in_use() {
        let asked: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        let any_port: SocketAddr = "127.0.0.1:0".parse().unwrap();

        let unbound = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        for addr in [asked, any_port] {
            let err = check_bound(&unbound, addr).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AddrInUse, "{addr}");
            assert_eq!(
                err.raw_os_error(),
                Some(crate::sys::errno::EADDRINUSE),
                "{addr}"
            );
        }

        let bound = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        bound.bind(&any_port.into()).unwrap();
        let local = bound.local_addr().unwrap().as_socket().unwrap();
        check_bound(&bound, local).unwrap();
        check_bound(&bound, any_port).unwrap();
        // On another port than the one asked for.
        let other = SocketAddr::new(local.ip(), local.port() ^ 1);
        let err = check_bound(&bound, other).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
    }

    /// A socket that refuses zero-copy sends, as a kernel TLS socket does
    /// with `EOPNOTSUPP`, gets the same bytes sent again with a copy and
    /// stops trying. A Unix socket refuses them the same way.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call sendmsg")]
    fn a_refused_zero_copy_send_is_sent_again_with_a_copy() {
        crate::block_on(async {
            let (a, b) = Socket::pair(Domain::UNIX, Type::STREAM, None).unwrap();
            let (a, b) = (StreamSocket::new(a), StreamSocket::new(b));
            let zero_copy = runtime::with_worker(|w| w.driver.borrow().zero_copy_min()).is_some();
            let data: Vec<u8> = (0..256 * 1024).map(|i| (i % 251) as u8).collect();
            let data: &'static [u8] = Box::leak(data.into_boxed_slice());
            for way in ["static", "rc", "vectored"] {
                a.zero_copy.set(true);
                if let Some(copied) = a.zero_copy_copied.get() {
                    copied.set(false);
                }
                let reader = crate::spawn({
                    let len = data.len();
                    let b = b.socket().try_clone().unwrap();
                    async move {
                        let b = StreamSocket::new(b);
                        let mut got = Vec::new();
                        while got.len() < len {
                            got.extend_from_slice(&b.recv().await.unwrap());
                        }
                        got
                    }
                });
                match way {
                    "static" => a.write_all(data).await.0.unwrap(),
                    "rc" => a.write_all(Rc::<[u8]>::from(data)).await.0.unwrap(),
                    _ => {
                        // A head that can change, copied aside, and a body
                        // that cannot.
                        let (head, body) = data.split_at(1000);
                        let pieces = vec![Cow::Owned(head.to_vec()), Cow::Borrowed(body)];
                        a.write_all_vectored(pieces).await.0.unwrap();
                    }
                }
                assert!(reader.await.unwrap() == data, "{way}");
                if zero_copy {
                    // Refused once, never tried again on this socket.
                    assert!(a.zero_copy_report(data.len()).is_none(), "{way}");
                    assert!(!a.zero_copy.get(), "{way}");
                }
            }
            drop(b);
        });
    }
}
