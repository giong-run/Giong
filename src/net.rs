//! TCP, UDP and Unix sockets on io_uring.
//!
//! * [`TcpListener::accept`] and [`UnixListener::accept`] use a **multishot
//!   accept**: one submission keeps producing connections.
//! * [`TcpStream::recv`], [`UnixStream::recv`] and [`UdpSocket::recv_from`]
//!   use a **multishot receive with provided buffers**: one submission per
//!   socket, and the kernel picks a buffer from the worker's shared pool only
//!   when data actually arrives — idle sockets pin no memory.
//! * Writes take ownership of the buffer and return it with the result.
//! * Descriptors are closed through the ring, batched with everything else.

mod addr;
mod dns;
mod udp;
mod unix;

pub use addr::{lookup_host, ToSocketAddrs};
pub use dns::{Resolver, ResolverConfig};
pub use udp::{Datagram, UdpSocket};
pub use unix::{UnixListener, UnixStream};

use std::cell::Cell;
use std::future::poll_fn;
use std::io;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::net::{Shutdown, SocketAddr, ToSocketAddrs as StdToSocketAddrs};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::task::{Context, Poll};

use io_uring::{cqueue, opcode, types};
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::buf::{IoBuf, IoBufMut, RecvBuf};
use crate::driver::{Driver, MultiKind, MultiPoll, BGID};
use crate::error::{raise, Code};
use crate::op::Op;
use crate::runtime::{self, Worker};

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

    /// Releases the slot. Returns `false` if the driver could not be reached.
    fn release(&self, w: &Worker) -> bool {
        match self.key.take() {
            Some((key, uid)) if uid == w.shared.uid => match w.driver.try_borrow_mut() {
                Ok(mut driver) => {
                    driver.drop_multi(key);
                    true
                }
                Err(_) => false,
            },
            _ => true,
        }
    }
}

/// Closes `sock` through the current worker's ring, or directly if there is none.
pub(crate) fn close_socket(sock: &mut ManuallyDrop<Socket>, slot: &MultiSlot) {
    let fd = sock.as_raw_fd();
    let closed = runtime::try_with_worker(|w| {
        // Cancel the multishot operation first: it holds a reference to the
        // socket, which would otherwise stay open after the close.
        slot.release(w);
        match w.driver.try_borrow_mut() {
            Ok(mut driver) => {
                driver.close_fd(fd);
                true
            }
            Err(_) => false,
        }
    })
    .unwrap_or(false);
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
        poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<StreamSocket>> {
        runtime::with_worker(|w| {
            let key = self.accept.key(w);
            let mut driver = w.driver.borrow_mut();
            loop {
                match driver.poll_multi(key, cx.waker()) {
                    MultiPoll::Item(res, _) if res >= 0 => {
                        // SAFETY: a fresh descriptor returned by accept.
                        let sock = unsafe { Socket::from_raw_fd(res) };
                        return Poll::Ready(Ok(StreamSocket::new(sock)));
                    }
                    MultiPoll::Item(res, _) => return Poll::Ready(Err(os_err(res))),
                    MultiPoll::Heap(_) => unreachable!("accept never queues heap data"),
                    MultiPoll::NeedArm => {
                        let sqe = opcode::AcceptMulti::new(types::Fd(self.sock.as_raw_fd()))
                            .flags(libc::SOCK_CLOEXEC)
                            .build();
                        driver.arm_multi(key, sqe);
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

/// A connected stream socket: what [`TcpStream`] and [`UnixStream`] share.
pub(crate) struct StreamSocket {
    sock: ManuallyDrop<Socket>,
    recv: MultiSlot,
    _not_send: PhantomData<*const ()>,
}

impl StreamSocket {
    pub(crate) fn new(sock: Socket) -> Self {
        Self {
            sock: ManuallyDrop::new(sock),
            recv: MultiSlot::new(MultiKind::Recv),
            _not_send: PhantomData,
        }
    }

    /// Connects `sock` to `addr` through the ring.
    pub(crate) async fn connect(sock: Socket, addr: SockAddr) -> io::Result<Self> {
        // Boxed so the address the SQE points at does not move with the future.
        let addr: Box<SockAddr> = Box::new(addr);
        let sqe = opcode::Connect::new(
            types::Fd(sock.as_raw_fd()),
            addr.as_ptr() as *const libc::sockaddr,
            addr.len(),
        )
        .build();
        let (res, _, _addr) = Op::new(sqe, addr).await;
        if res < 0 {
            return Err(os_err(res));
        }
        Ok(Self::new(sock))
    }

    pub(crate) fn socket(&self) -> &Socket {
        &self.sock
    }

    pub(crate) async fn recv(&self) -> io::Result<RecvBuf> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<io::Result<RecvBuf>> {
        runtime::with_worker(|w| {
            let key = self.recv.key(w);
            let mut driver = w.driver.borrow_mut();
            loop {
                match driver.poll_multi(key, cx.waker()) {
                    MultiPoll::Item(res, flags) => {
                        let bid = cqueue::buffer_select(flags);
                        return Poll::Ready(match (res, bid) {
                            (n, Some(bid)) if n > 0 => {
                                Ok(RecvBuf::pooled(driver.pool().clone(), bid, n as usize))
                            }
                            (n, bid) => {
                                // No payload: hand the buffer (if any) straight back.
                                if let Some(bid) = bid {
                                    driver.pool().recycle(bid);
                                }
                                match n {
                                    0 => Ok(RecvBuf::eof()),
                                    n if n < 0 => Err(os_err(n)),
                                    _ => Err(unexpected("io_uring returned data without a buffer")),
                                }
                            }
                        });
                    }
                    MultiPoll::Heap(data) => return Poll::Ready(Ok(RecvBuf::heap(data))),
                    MultiPoll::NeedArm => {
                        if !wait_for_pool(&mut driver, cx) {
                            return Poll::Pending;
                        }
                        let sqe =
                            opcode::RecvMulti::new(types::Fd(self.sock.as_raw_fd()), BGID).build();
                        driver.arm_multi(key, sqe);
                    }
                    MultiPoll::Pending => return Poll::Pending,
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
        let sqe =
            opcode::Recv::new(types::Fd(self.sock.as_raw_fd()), buf.as_mut_ptr(), len).build();
        let (res, _, mut buf) = Op::new(sqe, buf).await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
        // SAFETY: the kernel wrote `res` bytes at the start of the buffer.
        unsafe { buf.set_init(res as usize) };
        (Ok(res as usize), buf)
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

    async fn send_from<B: IoBuf>(&self, buf: B, offset: usize) -> (io::Result<usize>, B) {
        debug_assert!(offset <= buf.len());
        let len = (buf.len() - offset).min(u32::MAX as usize) as u32;
        // SAFETY: `offset <= len`, so the pointer stays within the buffer.
        let ptr = unsafe { buf.as_ptr().add(offset) };
        let sqe = opcode::Send::new(types::Fd(self.sock.as_raw_fd()), ptr, len)
            .flags(libc::MSG_NOSIGNAL)
            .build();
        let (res, _, buf) = Op::new(sqe, buf).await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }
}

impl Drop for StreamSocket {
    fn drop(&mut self) {
        close_socket(&mut self.sock, &self.recv);
    }
}

/// Makes sure the kernel has a pool buffer to receive into before a multishot
/// receive is armed. Returns `false` if the caller has to wait for one (its
/// waker is registered with the pool).
pub(crate) fn wait_for_pool(driver: &mut Driver, cx: &mut Context<'_>) -> bool {
    if driver.pool().free() == 0 {
        // The pool is exhausted. Reclaim buffers that hold unread data of
        // other sockets ...
        driver.evacuate();
    }
    if driver.pool().free() == 0 {
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
}

impl TcpListener {
    /// Binds to `addr` with `SO_REUSEADDR` and `SO_REUSEPORT` set, so every
    /// worker can bind its own listener to the same port and let the kernel
    /// spread incoming connections across cores.
    pub fn bind<A: StdToSocketAddrs>(addr: A) -> io::Result<Self> {
        let mut last_err = None;
        for addr in addr.to_socket_addrs()? {
            match Self::bind_addr(addr) {
                Ok(listener) => return Ok(listener),
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(unresolved))
    }

    fn bind_addr(addr: SocketAddr) -> io::Result<Self> {
        let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        sock.set_reuse_address(true)?;
        sock.set_reuse_port(true)?;
        sock.bind(&addr.into())?;
        sock.listen(4096)?;
        Ok(Self {
            inner: ListenSocket::new(sock),
        })
    }

    /// Wraps a listener created by the standard library.
    pub fn from_std(listener: std::net::TcpListener) -> Self {
        // SAFETY: we take over the descriptor that `listener` owned.
        let sock = unsafe { Socket::from_raw_fd(listener.into_raw_fd()) };
        Self {
            inner: ListenSocket::new(sock),
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
        let inner = self.inner.accept().await?;
        Ok(TcpStream { inner })
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

fn unresolved() -> io::Error {
    no_addresses()
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

    /// Reads into `buf` (from its start, up to its capacity) with a one-off
    /// receive, returning the buffer with its length set to the bytes read.
    /// `Ok(0)` means the peer closed the connection.
    ///
    /// Prefer [`recv`](Self::recv) for servers. The two must not be mixed on
    /// one stream: once `recv` has been used, `read` fails with `InvalidInput`.
    pub async fn read<B: IoBufMut>(&self, buf: B) -> (io::Result<usize>, B) {
        self.inner.read(buf).await
    }

    /// Writes some bytes of `buf`, returning how many were written.
    pub async fn write<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        self.inner.write(buf).await
    }

    /// Writes all of `buf`. The buffer is handed back so it can be reused.
    pub async fn write_all<B: IoBuf>(&self, buf: B) -> (io::Result<()>, B) {
        self.inner.write_all(buf).await
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
