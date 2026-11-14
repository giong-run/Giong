//! The giong streams `Compat` can wrap, and how it drives each of them.

use std::future::Future;
use std::io;
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
                Box::pin(async move { stream.recv().await })
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
