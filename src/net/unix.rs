//! Unix domain stream sockets.

use std::io;
use std::mem::ManuallyDrop;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{SocketAddr, UnixStream as StdStream};
use std::path::Path;
use std::task::{Context, Poll};

use socket2::{Domain, SockAddr, Socket, Type};

use super::{ListenSocket, StreamSocket};
use crate::buf::{IoBuf, IoBufMut, RecvBuf};

/// Lets the standard library read an address of `sock`: it knows how to
/// represent all three kinds (path, abstract, unnamed).
fn with_std<T>(sock: &Socket, f: impl FnOnce(&StdStream) -> io::Result<T>) -> io::Result<T> {
    // SAFETY: the descriptor is open for the duration of the call, and
    // `ManuallyDrop` keeps the borrowed wrapper from closing it.
    let std = ManuallyDrop::new(unsafe { StdStream::from_raw_fd(sock.as_raw_fd()) });
    f(&std)
}

fn new_socket() -> io::Result<Socket> {
    Socket::new(Domain::UNIX, Type::STREAM, None)
}

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
    pub fn bind<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let sock = new_socket()?;
        sock.bind(&SockAddr::unix(path)?)?;
        sock.listen(4096)?;
        Ok(Self {
            inner: ListenSocket::new(sock),
        })
    }

    /// Wraps a listener created by the standard library.
    pub fn from_std(listener: std::os::unix::net::UnixListener) -> Self {
        // SAFETY: we take over the descriptor that `listener` owned.
        let sock = unsafe { Socket::from_raw_fd(listener.into_raw_fd()) };
        Self {
            inner: ListenSocket::new(sock),
        }
    }

    /// The address this listener is bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        with_std(self.inner.socket(), StdStream::local_addr)
    }

    /// Waits for the next incoming connection.
    ///
    /// Cancel safe: dropping the future loses no connection.
    pub async fn accept(&self) -> io::Result<UnixStream> {
        let inner = self.inner.accept().await?;
        Ok(UnixStream { inner })
    }
}

impl AsRawFd for UnixListener {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.socket().as_raw_fd()
    }
}

impl std::fmt::Debug for UnixListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixListener")
            .field("fd", &self.as_raw_fd())
            .finish()
    }
}

/// A Unix domain stream connection. The API is that of
/// [`TcpStream`](super::TcpStream).
pub struct UnixStream {
    inner: StreamSocket,
}

impl UnixStream {
    /// Connects to the socket at `path`.
    pub async fn connect<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        let addr = SockAddr::unix(path)?;
        let inner = StreamSocket::connect(new_socket()?, addr).await?;
        Ok(Self { inner })
    }

    /// Creates two connected sockets.
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = Socket::pair(Domain::UNIX, Type::STREAM, None)?;
        let wrap = |sock| Self {
            inner: StreamSocket::new(sock),
        };
        Ok((wrap(a), wrap(b)))
    }

    /// Wraps a connected stream created by the standard library.
    pub fn from_std(stream: std::os::unix::net::UnixStream) -> Self {
        // SAFETY: we take over the descriptor that `stream` owned.
        let sock = unsafe { Socket::from_raw_fd(stream.into_raw_fd()) };
        Self {
            inner: StreamSocket::new(sock),
        }
    }

    /// Receives the next chunk of data. **An empty buffer means the peer
    /// closed the connection.**
    ///
    /// Cancel safe, like [`TcpStream::recv`](super::TcpStream::recv).
    pub async fn recv(&self) -> io::Result<RecvBuf> {
        self.inner.recv().await
    }

    /// The poll form of [`recv`](Self::recv); see
    /// [`TcpStream::poll_recv`](super::TcpStream::poll_recv).
    pub fn poll_recv(&self, cx: &mut Context<'_>) -> Poll<io::Result<RecvBuf>> {
        self.inner.poll_recv(cx)
    }

    /// Reads into `buf` with a one-off receive; see
    /// [`TcpStream::read`](super::TcpStream::read). Must not be mixed with
    /// [`recv`](Self::recv) on one stream.
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

    /// Writes some bytes of `bufs` with one system call; see
    /// [`TcpStream::write_vectored`](super::TcpStream::write_vectored).
    pub async fn write_vectored<B: IoBuf>(&self, bufs: Vec<B>) -> (io::Result<usize>, Vec<B>) {
        self.inner.write_vectored(bufs).await
    }

    /// Writes all of `bufs` without copying them into one buffer; see
    /// [`TcpStream::write_all_vectored`](super::TcpStream::write_all_vectored).
    pub async fn write_all_vectored<B: IoBuf>(&self, bufs: Vec<B>) -> (io::Result<()>, Vec<B>) {
        self.inner.write_all_vectored(bufs).await
    }

    /// Shuts down the read half, the write half, or both.
    pub fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        self.inner.socket().shutdown(how)
    }

    /// The address of the peer (unnamed for most clients).
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        with_std(self.inner.socket(), StdStream::peer_addr)
    }

    /// The local address of this connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        with_std(self.inner.socket(), StdStream::local_addr)
    }
}

impl AsRawFd for UnixStream {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.socket().as_raw_fd()
    }
}

impl std::fmt::Debug for UnixStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UnixStream")
            .field("fd", &self.as_raw_fd())
            .finish()
    }
}
