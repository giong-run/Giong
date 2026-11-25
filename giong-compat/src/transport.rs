//! The giong streams `Compat` can wrap, and how it drives each of them.

use std::future::Future;
use std::io::{self, IoSlice};
use std::net::Shutdown;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use giong::net::{TcpStream, UnixStream};
use giong::RecvBuf;

/// A send of a whole buffer, which hands the buffer back.
pub(crate) type SendFuture = Pin<Box<dyn Future<Output = (io::Result<()>, Vec<u8>)>>>;
/// Closing the sending direction, when it takes more than a system call.
pub(crate) type CloseFuture = Pin<Box<dyn Future<Output = io::Result<()>>>>;

/// A giong stream that [`Compat`](crate::Compat) can wrap:
/// [`TcpStream`], [`UnixStream`], and `giong_tls::TlsStream` with the `tls`
/// feature.
///
/// The trait is sealed: its methods are internal, and other types cannot
/// implement it.
pub trait Transport: sealed::Sealed {}

/// Received bytes: in a buffer of the worker's pool, or on the heap.
pub enum Chunk {
    Pool(RecvBuf),
    Heap(Vec<u8>),
}

pub(crate) mod sealed {
    use super::*;

    pub trait Sealed: Sized + 'static {
        /// The state of a receive that may be in progress.
        type Receiver: Default + Unpin;

        /// The type's name, for `Debug`.
        const NAME: &'static str;

        /// The next chunk of received bytes; an empty one is the end of the
        /// stream. Must be cancel safe: the adapter may never call it again.
        fn poll_chunk(
            this: &Rc<Self>,
            receiver: &mut Self::Receiver,
            cx: &mut Context<'_>,
        ) -> Poll<io::Result<Chunk>>;

        /// Sends all of `buf`.
        fn send(this: Rc<Self>, buf: Vec<u8>) -> SendFuture;

        /// Sends what the socket takes of `bufs` at once, straight from the
        /// caller's memory, without waiting: `None` if it takes nothing now
        /// (or the type cannot do that). Called only while nothing else is
        /// being sent.
        fn send_now(&self, _bufs: &[IoSlice<'_>]) -> Option<io::Result<usize>> {
            None
        }

        /// Closes the sending direction: `None` when that is done at once.
        fn close(this: &Rc<Self>) -> io::Result<Option<CloseFuture>>;

        /// Turns off Nagle's algorithm on a TCP connection: `Compat`
        /// already gathers small writes until a flush, so holding a sent
        /// segment back for an acknowledgement only adds delay (40 ms per
        /// delayed ACK).
        fn no_delay(&self) {}
    }
}

impl Chunk {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Chunk::Pool(buf) => buf,
            Chunk::Heap(buf) => buf,
        }
    }
}

/// `shutdown(2)` fails with `ENOTCONN` when the peer has already reset or
/// closed the connection, a race the caller cannot avoid: the sending
/// direction is closed either way. Tokio's streams report success then, and
/// the crates written for them expect it.
fn not_connected_is_closed(res: io::Result<()>) -> io::Result<()> {
    match res {
        Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()),
        res => res,
    }
}

/// One `sendmsg(2)` that does not wait, for a large write that nothing is
/// buffered before: the kernel copies straight from the caller's slices, as
/// Tokio's streams do, instead of `Compat` copying them into its buffer
/// first and the kernel copying them again.
#[cfg(unix)]
fn send_now(fd: std::os::fd::RawFd, bufs: &[IoSlice<'_>]) -> Option<io::Result<usize>> {
    // SAFETY: zeroed is a valid `msghdr` (no address, no control data).
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    // `IoSlice` is ABI compatible with `iovec`; `sendmsg` only reads them.
    msg.msg_iov = bufs.as_ptr() as *mut libc::iovec;
    // The most slices one call takes (`UIO_MAXIOV`, `IOV_MAX`): 1024 on
    // Linux and macOS.
    const MAX_SLICES: usize = 1024;
    // Off Linux the socket has `SO_NOSIGPIPE` instead of the flag, and is
    // non-blocking itself (giong makes every stream so on macOS, where
    // `MSG_DONTWAIT` does not keep a send from waiting).
    #[cfg(target_os = "linux")]
    const FLAGS: libc::c_int = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
    #[cfg(target_os = "macos")]
    const FLAGS: libc::c_int = libc::MSG_DONTWAIT;
    msg.msg_iovlen = bufs.len().min(MAX_SLICES) as _;
    // SAFETY: `msg` points at `bufs`, which outlive the call.
    let n = unsafe { libc::sendmsg(fd, &msg, FLAGS) };
    if n >= 0 {
        return Some(Ok(n as usize));
    }
    let e = io::Error::last_os_error();
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => None,
        _ => Some(Err(e)),
    }
}

/// One `WSASend` that does not wait (giong's streams are non-blocking from
/// the start on Windows), for the same reason as on Unix. Without an
/// `OVERLAPPED` it is a plain call: nothing reaches the completion port.
#[cfg(windows)]
fn send_now(
    socket: std::os::windows::io::RawSocket,
    bufs: &[IoSlice<'_>],
) -> Option<io::Result<usize>> {
    use windows_sys::Win32::Networking::WinSock::{WSAGetLastError, WSASend, WSABUF};
    // The most slices one call is given, as on Unix.
    const MAX_SLICES: usize = 1024;
    // The most bytes one call hands Winsock, as giong's driver does: Winsock
    // takes the whole of a send whenever its buffer is not yet full, so a
    // larger one would put all of it in the kernel for a peer that may
    // never read it.
    const MAX_BYTES: u32 = 1 << 20;
    let mut room = MAX_BYTES;
    let mut cut: Vec<WSABUF> = Vec::new();
    for b in bufs.iter().take(MAX_SLICES) {
        if room == 0 {
            break;
        }
        let len = (b.len().min(u32::MAX as usize) as u32).min(room);
        room -= len;
        cut.push(WSABUF {
            len,
            buf: b.as_ptr().cast_mut(),
        });
    }
    let mut sent = 0u32;
    // SAFETY: the pieces point at the caller's slices, which outlive the
    // call; `WSASend` only reads them. No overlapped structure or routine
    // is passed.
    let rc = unsafe {
        WSASend(
            socket as usize,
            cut.as_ptr(),
            cut.len() as u32,
            &mut sent,
            0,
            std::ptr::null_mut(),
            None,
        )
    };
    if rc == 0 {
        return Some(Ok(sent as usize));
    }
    // SAFETY: reads the calling thread's last Winsock error.
    let e = io::Error::from_raw_os_error(unsafe { WSAGetLastError() });
    match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => None,
        _ => Some(Err(e)),
    }
}

/// The socket of a giong stream, as [`send_now`] takes it.
#[cfg(unix)]
fn raw<T: std::os::fd::AsRawFd>(stream: &T) -> std::os::fd::RawFd {
    stream.as_raw_fd()
}

#[cfg(windows)]
fn raw<T: std::os::windows::io::AsRawSocket>(stream: &T) -> std::os::windows::io::RawSocket {
    stream.as_raw_socket()
}

macro_rules! socket_transport {
    ($ty:ident, $no_delay:expr) => {
        impl Transport for $ty {}

        impl sealed::Sealed for $ty {
            type Receiver = ();
            const NAME: &'static str = stringify!($ty);

            fn poll_chunk(
                this: &Rc<Self>,
                _: &mut (),
                cx: &mut Context<'_>,
            ) -> Poll<io::Result<Chunk>> {
                // The multishot receive keeps running between polls; nothing
                // is in flight on our side, so there is nothing to cancel.
                this.poll_recv(cx).map_ok(Chunk::Pool)
            }

            fn send(this: Rc<Self>, buf: Vec<u8>) -> SendFuture {
                Box::pin(async move { this.write_all(buf).await })
            }

            fn close(this: &Rc<Self>) -> io::Result<Option<CloseFuture>> {
                not_connected_is_closed(this.shutdown(Shutdown::Write)).map(|()| None)
            }

            fn send_now(&self, bufs: &[IoSlice<'_>]) -> Option<io::Result<usize>> {
                send_now(raw(self), bufs)
            }

            fn no_delay(&self) {
                ($no_delay)(self)
            }
        }
    };
}

socket_transport!(TcpStream, |tcp: &TcpStream| {
    // An error only means the option stays as it was.
    let _ = tcp.set_nodelay(true);
});
socket_transport!(UnixStream, |_: &UnixStream| {});

#[cfg(feature = "tls")]
mod tls {
    use super::*;
    use giong_tls::TlsStream;

    type RecvFuture = Pin<Box<dyn Future<Output = io::Result<Vec<u8>>>>>;

    impl Transport for TlsStream {}

    impl sealed::Sealed for TlsStream {
        type Receiver = Option<RecvFuture>;
        const NAME: &'static str = "TlsStream";

        fn poll_chunk(
            this: &Rc<Self>,
            receiver: &mut Option<RecvFuture>,
            cx: &mut Context<'_>,
        ) -> Poll<io::Result<Chunk>> {
            // `TlsStream::recv` is cancel safe, so dropping this future with
            // the adapter loses nothing.
            let recv = receiver.get_or_insert_with(|| {
                let stream = this.clone();
                Box::pin(async move { stream.recv().await.map(giong_tls::Received::into_vec) })
            });
            let res = std::task::ready!(recv.as_mut().poll(cx));
            *receiver = None;
            Poll::Ready(res.map(Chunk::Heap))
        }

        fn send(this: Rc<Self>, buf: Vec<u8>) -> SendFuture {
            Box::pin(async move { this.write_all(buf).await })
        }

        fn close(this: &Rc<Self>) -> io::Result<Option<CloseFuture>> {
            let stream = this.clone();
            Ok(Some(Box::pin(async move {
                not_connected_is_closed(stream.shutdown().await)
            })))
        }

        fn no_delay(&self) {
            let _ = self.tcp().set_nodelay(true);
        }
    }
}
