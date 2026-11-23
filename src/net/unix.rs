//! Unix domain sockets: streams, datagrams, and passing descriptors
//! between processes over either.
//!
//! On Windows (10 version 1803 and later, Server 2019 and later) the
//! streams are Winsock's `AF_UNIX` stream sockets: a path names the socket
//! as on Unix, and the server's file is left behind when the listener
//! closes, as on Unix. Windows has no abstract names, no datagram sockets,
//! no passing of handles or credentials over them and no `socketpair`:
//! [`UnixStream::pair`] connects two sockets through a listener of its own
//! instead.

use std::cell::Cell;
use std::io;
#[cfg(unix)]
use std::mem::ManuallyDrop;
use std::net::Shutdown;
#[cfg(unix)]
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{SocketAddr, UnixStream as StdStream};
use std::path::Path;
use std::task::{Context, Poll};

#[cfg(unix)]
use socket2::{Domain, SockAddr, Socket, Type};

use super::{ListenSocket, StreamSocket};
use crate::buf::{IoBuf, IoBufMut, RecvBuf};
use crate::error::{raise, Code};

// Descriptor passing, peer credentials, abstract names and datagram
// sockets: Linux only for now.
#[cfg(target_os = "linux")]
use {
    super::fds::{self, PeerCred},
    super::{close_socket, os_err, MultiSlot},
    crate::driver::{Io, MultiKind},
    crate::op::Op,
    crate::runtime,
    std::marker::PhantomData,
    std::os::fd::OwnedFd,
};

/// The address of a Unix domain socket, as [`UnixListener::local_addr`],
/// [`UnixStream::local_addr`] and [`UnixStream::peer_addr`] return it: the
/// standard library's [`std::os::unix::net::SocketAddr`] on Unix.
#[cfg(unix)]
pub type UnixSocketAddr = std::os::unix::net::SocketAddr;

#[cfg(windows)]
pub use super::unix_windows::UnixSocketAddr;

/// Lets the standard library read an address of `sock`: it knows how to
/// represent all three kinds (path, abstract, unnamed).
#[cfg(unix)]
fn with_std<T>(sock: &Socket, f: impl FnOnce(&StdStream) -> io::Result<T>) -> io::Result<T> {
    // SAFETY: the descriptor is open for the duration of the call, and
    // `ManuallyDrop` keeps the borrowed wrapper from closing it.
    let std = ManuallyDrop::new(unsafe { StdStream::from_raw_fd(sock.as_raw_fd()) });
    f(&std)
}

#[cfg(unix)]
fn local_addr(sock: &Socket) -> io::Result<UnixSocketAddr> {
    with_std(sock, StdStream::local_addr)
}

#[cfg(unix)]
fn peer_addr(sock: &Socket) -> io::Result<UnixSocketAddr> {
    with_std(sock, StdStream::peer_addr)
}

#[cfg(unix)]
fn new_socket() -> io::Result<Socket> {
    Socket::new(Domain::UNIX, Type::STREAM, None)
}

/// The address of the socket file `path`.
#[cfg(unix)]
fn socket_addr(path: &Path) -> io::Result<SockAddr> {
    SockAddr::unix(path)
}

/// Binds a new socket to `path`.
#[cfg(unix)]
fn bind_socket(path: &Path) -> io::Result<Socket> {
    let sock = new_socket()?;
    sock.bind(&socket_addr(path)?)?;
    Ok(sock)
}

#[cfg(windows)]
use super::unix_windows::{bind_socket, local_addr, new_socket, peer_addr, socket_addr};

/// A Unix domain socket listening for connections.
///
/// Like the standard library's listener, dropping it does not remove the
/// socket file, and [`bind`](Self::bind) fails with `AddrInUse` if the file
/// already exists.
pub struct UnixListener {
    inner: ListenSocket,
}

impl UnixListener {
    /// Creates the socket file `path` and listens on it.
    ///
    /// On Windows, a file already at `path` fails with
    /// [`Code::UnixSocketInUse`](crate::error::Code::UnixSocketInUse) and a
    /// path of 108 bytes or more (in UTF-8) with
    /// [`Code::UnixPathTooLong`](crate::error::Code::UnixPathTooLong).
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let sock = bind_socket(path.as_ref())?;
        sock.listen(4096)?;
        Ok(Self {
            inner: ListenSocket::new(sock),
        })
    }

    /// Listens on `addr`: a path, or on Linux an abstract name made with
    /// [`SocketAddrExt::from_abstract_name`](std::os::linux::net::SocketAddrExt::from_abstract_name),
    /// which has no file and disappears with the last socket bound to it.
    #[cfg(target_os = "linux")]
    pub fn bind_addr(addr: &SocketAddr) -> io::Result<Self> {
        let sock = new_socket()?;
        sock.bind(&fds::sock_addr(addr)?)?;
        sock.listen(4096)?;
        Ok(Self {
            inner: ListenSocket::new(sock),
        })
    }

    /// Wraps a listener created by the standard library. Unix only: the
    /// standard library has no Unix sockets on Windows.
    #[cfg(unix)]
    pub fn from_std(listener: std::os::unix::net::UnixListener) -> Self {
        // SAFETY: we take over the descriptor that `listener` owned.
        let sock = unsafe { Socket::from_raw_fd(listener.into_raw_fd()) };
        Self {
            inner: ListenSocket::new(sock),
        }
    }

    /// The address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<UnixSocketAddr> {
        local_addr(self.inner.socket())
    }

    /// Waits for the next incoming connection.
    ///
    /// Cancel safe: dropping the future loses no connection.
    pub async fn accept(&self) -> io::Result<UnixStream> {
        let inner = self.inner.accept().await?;
        Ok(UnixStream::wrap(inner))
    }
}

#[cfg(unix)]
impl AsRawFd for UnixListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.socket().as_raw_fd()
    }
}

#[cfg(unix)]
impl AsFd for UnixListener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.socket().as_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for UnixListener {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        std::os::windows::io::AsRawSocket::as_raw_socket(self.inner.socket())
    }
}

impl std::fmt::Debug for UnixListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("UnixListener");
        #[cfg(unix)]
        d.field("fd", &self.as_raw_fd());
        #[cfg(windows)]
        d.field(
            "socket",
            &std::os::windows::io::AsRawSocket::as_raw_socket(self),
        );
        d.finish()
    }
}

/// A Unix domain stream connection. The API is that of
/// [`TcpStream`](super::TcpStream).
///
/// It can also pass open descriptors (files, sockets, pipes) to the process
/// at the other end: [`send_with_fds`](Self::send_with_fds) and
/// [`recv_with_fds`](Self::recv_with_fds).
pub struct UnixStream {
    inner: StreamSocket,
    /// Set once descriptors have been received: from then on `recv`, whose
    /// multishot receive would let the kernel close descriptors that arrive,
    /// is refused.
    passes_fds: Cell<bool>,
}

impl UnixStream {
    fn wrap(inner: StreamSocket) -> Self {
        Self {
            inner: inner.unix(),
            passes_fds: Cell::new(false),
        }
    }

    /// Connects to the socket at `path`.
    ///
    /// Cancel safe: if the future is dropped, the socket is closed and no
    /// connection is left behind.
    ///
    /// On Windows, a path of 108 bytes or more (in UTF-8) fails with
    /// [`Code::UnixPathTooLong`](crate::error::Code::UnixPathTooLong).
    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let addr = socket_addr(path.as_ref())?;
        let inner = StreamSocket::connect_new(new_socket, addr).await?;
        Ok(Self::wrap(inner))
    }

    /// Connects to `addr`: a path, or on Linux an abstract name (see
    /// [`UnixListener::bind_addr`]).
    ///
    /// Cancel safe, as [`connect`](Self::connect).
    #[cfg(target_os = "linux")]
    pub async fn connect_addr(addr: &SocketAddr) -> io::Result<Self> {
        let addr = fds::sock_addr(addr)?;
        let inner = StreamSocket::connect_new(new_socket, addr).await?;
        Ok(Self::wrap(inner))
    }

    /// Creates two connected sockets.
    ///
    /// Windows has no `socketpair`: there the pair is made by connecting
    /// to a listener bound to a fresh path in the temporary directory,
    /// whose file is removed before this returns. Both sockets behave as
    /// a pair made on Unix, but their addresses are named after that path
    /// instead of unnamed.
    pub fn pair() -> io::Result<(Self, Self)> {
        #[cfg(unix)]
        let (a, b) = Socket::pair(Domain::UNIX, Type::STREAM, None)?;
        #[cfg(windows)]
        let (a, b) = super::unix_windows::pair()?;
        Ok((
            Self::wrap(StreamSocket::new(a)),
            Self::wrap(StreamSocket::new(b)),
        ))
    }

    /// Wraps a connected stream created by the standard library. Unix only:
    /// the standard library has no Unix sockets on Windows.
    #[cfg(unix)]
    pub fn from_std(stream: std::os::unix::net::UnixStream) -> Self {
        // SAFETY: we take over the descriptor that `stream` owned.
        let sock = unsafe { Socket::from_raw_fd(stream.into_raw_fd()) };
        // macOS has no per-send flag against `SIGPIPE`: the socket carries
        // the option. socket2 sets it on the sockets it creates; this covers
        // one that came from somewhere else (`from_std`).
        #[cfg(target_os = "macos")]
        drop(sock.set_nosigpipe(true));
        Self::wrap(StreamSocket::new(sock))
    }

    /// Receives the next chunk of data. **An empty buffer means the peer
    /// closed the connection.**
    ///
    /// Cancel safe, like [`TcpStream::recv`](super::TcpStream::recv).
    ///
    /// Not on a stream that receives descriptors: once
    /// [`recv_with_fds`](Self::recv_with_fds) has been used, this fails with
    /// [`Code::MixedReads`](crate::error::Code::MixedReads).
    pub async fn recv(&self) -> io::Result<RecvBuf> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    /// The poll form of [`recv`](Self::recv); see
    /// [`TcpStream::poll_recv`](super::TcpStream::poll_recv).
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<io::Result<RecvBuf>> {
        if self.passes_fds.get() {
            return Poll::Ready(Err(raise(
                Code::MixedReads,
                "recv cannot be used on a stream that receives descriptors",
                "keep using recv_with_fds on this stream: recv's multishot receive would let the \
                 kernel close descriptors that arrive. Use a separate stream for plain data.",
            )));
        }
        self.inner.poll_recv(cx)
    }

    /// Sends some bytes of `buf` together with copies of the open
    /// descriptors `fds`, which the process at the other end receives with
    /// [`recv_with_fds`](Self::recv_with_fds) (`SCM_RIGHTS`). Returns how
    /// many bytes were sent.
    ///
    /// * The descriptors stay open here: the peer gets its own copies,
    ///   sharing the same open files and sockets. Close yours when you no
    ///   longer need them.
    /// * They travel with the first byte. If fewer bytes went than `buf`
    ///   holds, send the rest with [`write_all`](Self::write_all).
    /// * `buf` must not be empty, and at most [`MAX_FDS`](super::MAX_FDS)
    ///   (253) descriptors go in one call; either mistake fails with
    ///   [`Code::FdPassing`](crate::error::Code::FdPassing) and sends
    ///   nothing.
    ///
    /// The send works on copies of the descriptors made when it starts, so
    /// closing one of yours while the send is in flight is safe.
    ///
    /// Not cancel safe: if the future is dropped, the bytes and descriptors
    /// may or may not have been sent.
    #[cfg(target_os = "linux")]
    pub async fn send_with_fds<B: IoBuf>(
        &self,
        buf: B,
        fds: &[BorrowedFd<'_>],
    ) -> (io::Result<usize>, B) {
        fds::send(self.as_raw_fd(), buf, fds, None).await
    }

    /// Reads into `buf`, and takes the descriptors that came with those
    /// bytes, if any (see [`send_with_fds`](Self::send_with_fds)). Returns
    /// the number of bytes read, 0 when the peer closed the connection, and
    /// the descriptors, now owned by this process and closed on `exec`.
    ///
    /// A read stops where descriptors were attached, so they always arrive
    /// with the first byte they were sent with. On a stream that passes
    /// descriptors, read only with this method: [`recv`](Self::recv) is then
    /// refused, and [`read`](Self::read) would let the kernel close
    /// descriptors that arrive.
    ///
    /// Not cancel safe: if the future is dropped, bytes the receive had
    /// already taken are lost; descriptors that came with them are closed,
    /// never leaked.
    #[cfg(target_os = "linux")]
    pub async fn recv_with_fds<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (io::Result<(usize, Vec<OwnedFd>)>, B) {
        if runtime::with_worker(|w| self.inner.recv.is_active(w)) {
            let err = raise(
                Code::MixedReads,
                "recv_with_fds cannot be used after recv on the same stream",
                "use recv_with_fds for every read on a stream that passes descriptors: recv's \
                 multishot receive lets the kernel close descriptors that arrive.",
            );
            return (Err(err), buf);
        }
        self.passes_fds.set(true);
        let (res, buf) = fds::recv(self.as_raw_fd(), buf, true, false).await;
        (res.map(|r| (r.len, r.fds)), buf)
    }

    /// The process ID, user ID and group ID of the process at the other end,
    /// as the kernel recorded them at `connect` (or `pair`): who is talking
    /// to this server.
    #[cfg(target_os = "linux")]
    pub fn peer_cred(&self) -> io::Result<PeerCred> {
        fds::peer_cred(self.as_raw_fd())
    }

    /// Reads into `buf` with a one-off receive; see
    /// [`TcpStream::read`](super::TcpStream::read). Must not be mixed with
    /// [`recv`](Self::recv) on one stream.
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
    /// Not cancel safe: if the future is dropped, part of the data may have
    /// been sent.
    pub async fn write_all<B: IoBuf>(&self, buf: B) -> (io::Result<()>, B) {
        self.inner.write_all(buf).await
    }

    /// Writes some bytes of `bufs` with one system call; see
    /// [`TcpStream::write_vectored`](super::TcpStream::write_vectored).
    ///
    /// Not cancel safe: if the future is dropped, some bytes may have been sent
    /// without the caller learning how many.
    pub async fn write_vectored<B: IoBuf>(&self, bufs: Vec<B>) -> (io::Result<usize>, Vec<B>) {
        self.inner.write_vectored(bufs).await
    }

    /// Writes all of `bufs` without copying them into one buffer; see
    /// [`TcpStream::write_all_vectored`](super::TcpStream::write_all_vectored).
    ///
    /// Not cancel safe: if the future is dropped, part of the data may have
    /// been sent.
    pub async fn write_all_vectored<B: IoBuf>(&self, bufs: Vec<B>) -> (io::Result<()>, Vec<B>) {
        self.inner.write_all_vectored(bufs).await
    }

    /// Shuts down the read half, the write half, or both. Shutting down the
    /// read half ends a [`recv`](Self::recv) that waits: it returns the end
    /// of the stream.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.shutdown(how)
    }

    /// The address of the peer (unnamed for most clients).
    pub fn peer_addr(&self) -> io::Result<UnixSocketAddr> {
        peer_addr(self.inner.socket())
    }

    /// The local address of this connection.
    pub fn local_addr(&self) -> io::Result<UnixSocketAddr> {
        local_addr(self.inner.socket())
    }
}

#[cfg(unix)]
impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.socket().as_raw_fd()
    }
}

#[cfg(unix)]
impl AsFd for UnixStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.inner.socket().as_fd()
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for UnixStream {
    fn as_raw_socket(&self) -> std::os::windows::io::RawSocket {
        std::os::windows::io::AsRawSocket::as_raw_socket(self.inner.socket())
    }
}

impl std::fmt::Debug for UnixStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("UnixStream");
        #[cfg(unix)]
        d.field("fd", &self.as_raw_fd());
        #[cfg(windows)]
        d.field(
            "socket",
            &std::os::windows::io::AsRawSocket::as_raw_socket(self),
        );
        d.finish()
    }
}

/// A Unix domain datagram socket: messages keep their boundaries, and none
/// is lost to a full buffer (the sender waits instead).
///
/// It can send to any bound socket ([`send_to`](Self::send_to)), or be
/// connected to one peer ([`connect`](Self::connect), [`send`](Self::send)).
/// Like [`UnixStream`], it can pass open descriptors to another process
/// ([`send_with_fds`](Self::send_with_fds),
/// [`recv_with_fds`](Self::recv_with_fds)).
///
/// Receives read into a buffer of yours, as
/// [`UnixStream::read`] does: give it room for the largest message, since
/// the rest of a longer one is discarded.
#[cfg(target_os = "linux")]
pub struct UnixDatagram {
    sock: ManuallyDrop<Socket>,
    /// Never armed: `close_socket` takes one.
    slot: MultiSlot,
    _not_send: PhantomData<*const ()>,
}

#[cfg(target_os = "linux")]
impl UnixDatagram {
    fn wrap(sock: Socket) -> Self {
        Self {
            sock: ManuallyDrop::new(sock),
            slot: MultiSlot::new(MultiKind::Recv),
            _not_send: PhantomData,
        }
    }

    fn new_socket() -> io::Result<Socket> {
        Socket::new(Domain::UNIX, Type::DGRAM, None)
    }

    /// Creates the socket file `path` and receives on it. Like
    /// [`UnixListener::bind`], fails with `AddrInUse` if the file exists.
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let sock = Self::new_socket()?;
        sock.bind(&SockAddr::unix(path)?)?;
        Ok(Self::wrap(sock))
    }

    /// Binds to `addr`: a path, or on Linux an abstract name (see
    /// [`UnixListener::bind_addr`]).
    pub fn bind_addr(addr: &SocketAddr) -> io::Result<Self> {
        let sock = Self::new_socket()?;
        sock.bind(&fds::sock_addr(addr)?)?;
        Ok(Self::wrap(sock))
    }

    /// A socket bound to no address, for sending. Peers see it as unnamed
    /// and cannot answer it unless it is connected to them.
    pub fn unbound() -> io::Result<Self> {
        Ok(Self::wrap(Self::new_socket()?))
    }

    /// Creates two connected sockets.
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = Socket::pair(Domain::UNIX, Type::DGRAM, None)?;
        Ok((Self::wrap(a), Self::wrap(b)))
    }

    /// Wraps a socket created by the standard library.
    pub fn from_std(socket: std::os::unix::net::UnixDatagram) -> Self {
        // SAFETY: we take over the descriptor that `socket` owned.
        Self::wrap(unsafe { Socket::from_raw_fd(socket.into_raw_fd()) })
    }

    /// Sets the only peer this socket sends to and receives from. Never
    /// waits: a datagram connect only records the address.
    pub fn connect<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        self.sock.connect(&SockAddr::unix(path)?)
    }

    /// [`connect`](Self::connect) to a path or an abstract name.
    pub fn connect_addr(&self, addr: &SocketAddr) -> io::Result<()> {
        self.sock.connect(&fds::sock_addr(addr)?)
    }

    /// Sends `buf` as one message to the connected peer. Returns its length.
    ///
    /// Not cancel safe: if the future is dropped, the message may or may
    /// not have been sent.
    pub async fn send<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        let io = Io::Send {
            fd: self.as_raw_fd(),
            buf: buf.as_ptr(),
            len: buf.len().min(u32::MAX as usize) as u32,
            flags: libc::MSG_NOSIGNAL,
            zero_copy: false,
        };
        // SAFETY: an `IoBuf`'s bytes stay put when it moves.
        let (res, _, buf) = unsafe { Op::new(io, buf) }.await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Sends `buf` as one message to the socket at `path`.
    ///
    /// Not cancel safe, as [`send`](Self::send).
    pub async fn send_to<B: IoBuf, P: AsRef<Path>>(
        &self,
        buf: B,
        path: P,
    ) -> (io::Result<usize>, B) {
        match SockAddr::unix(path) {
            Ok(to) => fds::send(self.as_raw_fd(), buf, &[], Some(&to)).await,
            Err(e) => (Err(e), buf),
        }
    }

    /// Sends `buf` as one message to `addr`, such as an address
    /// [`recv_from`](Self::recv_from) returned.
    ///
    /// Not cancel safe, as [`send`](Self::send).
    pub async fn send_to_addr<B: IoBuf>(
        &self,
        buf: B,
        addr: &SocketAddr,
    ) -> (io::Result<usize>, B) {
        match fds::sock_addr(addr) {
            Ok(to) => fds::send(self.as_raw_fd(), buf, &[], Some(&to)).await,
            Err(e) => (Err(e), buf),
        }
    }

    /// Receives one message into `buf`. Returns its length, cut to `buf`'s
    /// capacity if it was longer.
    ///
    /// Cancel safe on the epoll driver; on io_uring a message the kernel
    /// had already delivered when the future was dropped is lost.
    pub async fn recv<B: IoBufMut>(&self, mut buf: B) -> (io::Result<usize>, B) {
        let cap = buf.capacity();
        let io = Io::Recv {
            fd: self.as_raw_fd(),
            buf: buf.as_mut_ptr(),
            len: cap.min(u32::MAX as usize) as u32,
        };
        // SAFETY: an `IoBufMut`'s bytes stay put when it moves.
        let (res, _, mut buf) = unsafe { Op::new(io, buf) }.await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
        let n = (res as usize).min(cap);
        // SAFETY: the kernel wrote `n` bytes at the start of the buffer.
        unsafe { buf.set_init(n) };
        (Ok(n), buf)
    }

    /// Receives one message into `buf`, and the address of its sender
    /// (unnamed if the sender is not bound).
    ///
    /// Cancel safety as for [`recv`](Self::recv).
    pub async fn recv_from<B: IoBufMut>(&self, buf: B) -> (io::Result<(usize, SocketAddr)>, B) {
        let (res, buf) = fds::recv(self.as_raw_fd(), buf, false, true).await;
        (
            res.map(|r| (r.len, r.from.expect("asked for the sender"))),
            buf,
        )
    }

    /// Sends `buf` as one message to the connected peer, together with
    /// copies of the descriptors `fds`. The same rules as
    /// [`UnixStream::send_with_fds`].
    ///
    /// Not cancel safe: if the future is dropped, the message and its
    /// descriptors may or may not have been sent.
    pub async fn send_with_fds<B: IoBuf>(
        &self,
        buf: B,
        fds: &[BorrowedFd<'_>],
    ) -> (io::Result<usize>, B) {
        fds::send(self.as_raw_fd(), buf, fds, None).await
    }

    /// Sends `buf` as one message to the socket at `path`, together with
    /// copies of the descriptors `fds`. The same rules as
    /// [`UnixStream::send_with_fds`].
    ///
    /// Not cancel safe: if the future is dropped, the message and its
    /// descriptors may or may not have been sent.
    pub async fn send_to_with_fds<B: IoBuf, P: AsRef<Path>>(
        &self,
        buf: B,
        fds: &[BorrowedFd<'_>],
        path: P,
    ) -> (io::Result<usize>, B) {
        match SockAddr::unix(path) {
            Ok(to) => fds::send(self.as_raw_fd(), buf, fds, Some(&to)).await,
            Err(e) => (Err(e), buf),
        }
    }

    /// Receives one message into `buf` and the descriptors sent with it,
    /// now owned by this process and closed on `exec`.
    ///
    /// Not cancel safe: on io_uring, a message the kernel had already
    /// delivered when the future was dropped is lost, and the descriptors
    /// that came with it are closed, never leaked.
    pub async fn recv_with_fds<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (io::Result<(usize, Vec<OwnedFd>)>, B) {
        let (res, buf) = fds::recv(self.as_raw_fd(), buf, true, false).await;
        (res.map(|r| (r.len, r.fds)), buf)
    }

    /// The credentials of the peer of a connected socket or a pair (see
    /// [`UnixStream::peer_cred`]).
    pub fn peer_cred(&self) -> io::Result<PeerCred> {
        fds::peer_cred(self.as_raw_fd())
    }

    /// The address this socket is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        with_std_datagram(&self.sock, std::os::unix::net::UnixDatagram::local_addr)
    }

    /// The address of the connected peer.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        with_std_datagram(&self.sock, std::os::unix::net::UnixDatagram::peer_addr)
    }

    /// Shuts down the read half, the write half, or both.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.sock.shutdown(how)
    }
}

#[cfg(target_os = "linux")]
fn with_std_datagram<T>(
    sock: &Socket,
    f: impl FnOnce(&std::os::unix::net::UnixDatagram) -> io::Result<T>,
) -> io::Result<T> {
    // SAFETY: the descriptor is open for the duration of the call, and
    // `ManuallyDrop` keeps the borrowed wrapper from closing it.
    let std = ManuallyDrop::new(unsafe {
        std::os::unix::net::UnixDatagram::from_raw_fd(sock.as_raw_fd())
    });
    f(&std)
}

#[cfg(target_os = "linux")]
impl Drop for UnixDatagram {
    fn drop(&mut self) {
        close_socket(&mut self.sock, &self.slot);
    }
}

#[cfg(target_os = "linux")]
impl AsRawFd for UnixDatagram {
    fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
impl AsFd for UnixDatagram {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.sock.as_fd()
    }
}

#[cfg(target_os = "linux")]
impl std::fmt::Debug for UnixDatagram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixDatagram")
            .field("fd", &self.as_raw_fd())
            .finish()
    }
}
