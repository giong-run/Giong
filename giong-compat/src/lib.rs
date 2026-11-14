//! # giong-compat
//!
//! The I/O traits of Tokio (`tokio::io::{AsyncRead, AsyncWrite,
//! AsyncBufRead}`) and of the futures crates (`futures_io::{AsyncRead,
//! AsyncWrite, AsyncBufRead}`) for giong's streams, so that protocol crates
//! which are generic over their transport run on giong: TLS (tokio-rustls,
//! futures-rustls), WebSocket (tokio-tungstenite), HTTP/2 (h2), hyper's
//! connection API, framing (tokio-util's codecs), and the like.
//!
//! ```no_run
//! use giong::net::TcpStream;
//! use giong_compat::Compat;
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! # giong::block_on(async {
//! let mut stream = Compat::new(TcpStream::connect("127.0.0.1:7000").await?);
//! stream.write_all(b"ping\n").await?;
//! stream.flush().await?; // writes are buffered until a flush or a read
//! let mut reply = [0; 5];
//! stream.read_exact(&mut reply).await?;
//! # Ok::<_, std::io::Error>(())
//! # }).unwrap();
//! ```
//!
//! ## When to use it, and when not
//!
//! **Use the native crate when there is one.** giong-http (HTTP/1.1 and
//! HTTP/2, server and client), giong-web, giong-tls, giong-postgres,
//! giong-mysql and giong-redis are built on giong's owned buffers and
//! batching and are faster than the same protocol through this adapter.
//! `Compat` is the bridge for protocols giong has no crate for yet.
//!
//! **It works for crates that take the stream from you and are driven by
//! you:** a function such as `tokio_rustls::TlsConnector::connect(name,
//! stream)`, `tokio_tungstenite::client_async(request, stream)`,
//! `h2::client::handshake(stream)` or
//! `hyper::client::conn::http1::handshake(io)`, which returns a future or a
//! connection object that your task (or a task you start with
//! [`giong::spawn`]) polls. The crate's tests run tokio-rustls,
//! futures-rustls, tokio-tungstenite, h2, hyper (HTTP/1) and tokio-util's
//! codecs this way.
//!
//! **It does not make a crate that needs a Tokio runtime work.** I/O traits
//! are all `Compat` provides. A crate does not work through it if it
//!
//! - opens its own connections with `tokio::net` (reqwest, tonic's
//!   `transport`, sqlx, redis-rs, most SDK clients): it panics with "there is
//!   no reactor running, must be called from the context of a Tokio 1.x
//!   runtime", or never finishes;
//! - uses Tokio's timers (`tokio::time::sleep`, `timeout`) or
//!   `tokio::spawn`: the same panic. hyper takes a timer and an executor
//!   from you instead, and its server needs a timer for
//!   `header_read_timeout`; the README shows one on giong's timers;
//! - requires the stream, or the futures it spawns, to be `Send` (it says
//!   `T: Send` or `+ Send + 'static` in its bounds): `Compat` is not
//!   `Send`, because giong's tasks stay on their worker. The compiler says
//!   so.
//!
//! Some of these take a custom connector or executor; with one built on
//! giong they can work, but each has to be tried. For everything else, run
//! the crate on a Tokio runtime in its own thread.
//!
//! ## What it costs
//!
//! giong reads into buffers from the worker's pool and writes buffers it
//! owns until the kernel is done; these traits read into, and write from,
//! memory the caller lends for one call. So `Compat` copies:
//!
//! - **reading**: every byte once more, from the pool buffer into the
//!   caller's buffer. Through `AsyncBufRead` (`fill_buf` and `consume`)
//!   there is no copy, as long as each chunk is consumed whole. When the
//!   caller takes part of a chunk, the rest moves to a heap buffer at once,
//!   so a slow reader never holds on to the shared pool.
//! - **writing**: every byte once, into a buffer of its own (up to
//!   [`write_buffer`](Compat::write_buffer) bytes, 64 KiB by default), sent
//!   with giong's `write_all`. One small allocation per send.
//!
//! The README has the numbers measured against giong's own API and against
//! Tokio.
//!
//! ## How it behaves
//!
//! - **Writes are buffered**, as with `BufWriter` or a TLS stream:
//!   `poll_write` copies the bytes and returns; they are sent on `flush`,
//!   when the buffer is full, or when a read on the same task is about to
//!   wait. Call `flush` (or `shutdown`) where the data must have been sent;
//!   every crate written for these traits already does. A writer in another
//!   task than the reader must flush: a read only sends what its own task
//!   wrote. While one buffer is being sent the next one fills, so a long
//!   transfer keeps the socket busy.
//! - **A failed send is reported by the next write, flush or shutdown**,
//!   with the kernel's error; later calls fail with
//!   [`GIONG-E1401`](giong::error::Code::CompatWriteFailed). Writing after
//!   `shutdown` fails with
//!   [`GIONG-E1402`](giong::error::Code::CompatShutDown).
//! - **Dropping a `Compat` discards what was written but not flushed**, like
//!   dropping a `BufWriter`.
//! - **One task reads and one task writes at a time.** `tokio::io::split`
//!   and `futures::io::AsyncReadExt::split` work, since nothing here needs
//!   `Send`.
//! - **Reading after the end of the stream** keeps returning 0 bytes.
//! - **Cancel safe**: a dropped read loses no data, and a write either
//!   copies bytes and says how many (possibly fewer than given), or copies
//!   nothing.

#![doc(
    html_logo_url = "https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-icon.svg",
    html_favicon_url = "https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-icon.svg"
)]
#![warn(missing_docs)]

#[cfg(not(any(feature = "tokio", feature = "futures-io")))]
compile_error!(
    "giong-compat implements no trait without its `tokio` or `futures-io` feature: enable one \
     of them, or keep the default features (both)"
);

mod transport;

use std::fmt;
use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::mem;
use std::rc::Rc;
use std::task::{ready, Context, Poll, Waker};

use giong::error::{Code, Error};

pub use transport::Transport;
use transport::{Chunk, CloseFuture, SendFuture};

/// The default [`write_buffer`](Compat::write_buffer) limit: 64 KiB.
pub const DEFAULT_WRITE_BUFFER: usize = 64 * 1024;

/// The largest [`write_buffer`](Compat::write_buffer) limit: 1 GiB.
pub const MAX_WRITE_BUFFER: usize = 1 << 30;

/// A giong stream seen through the Tokio and futures-io I/O traits.
///
/// Wraps a [`TcpStream`](giong::net::TcpStream), a
/// [`UnixStream`](giong::net::UnixStream) or, with the `tls` feature, a
/// `giong_tls::TlsStream`, and implements `AsyncRead`, `AsyncWrite` and
/// `AsyncBufRead` from `tokio::io` (feature `tokio`) and from `futures_io`
/// (feature `futures-io`); both features are on by default.
///
/// See the [crate documentation](crate) for which crates this makes usable,
/// what it costs, and how writes are buffered. `Compat` is `Unpin` and not
/// `Send`.
pub struct Compat<S: Transport> {
    stream: Rc<S>,
    read: ReadState<S>,
    write: WriteState,
}

struct ReadState<S: Transport> {
    /// A receive that may be in progress (TLS only).
    receiver: S::Receiver,
    /// Received bytes not yet read.
    unread: Unread,
    eof: bool,
}

/// Received bytes not yet read, and how far they have been read.
enum Unread {
    None,
    /// Still in the pool buffer it arrived in: only between `poll_fill_buf`
    /// and `consume`, or until a read takes all of it.
    Chunk(Chunk, usize),
    /// Moved to the heap, so that the pool buffer could go back.
    Heap(Vec<u8>, usize),
}

struct WriteState {
    limit: usize,
    /// Accepted bytes not yet handed to the stream.
    buf: Vec<u8>,
    /// The send in progress.
    sending: Option<SendFuture>,
    /// The waker the send in progress was last polled with: its task drives
    /// it, and a read from another task leaves it alone.
    driver: Option<Waker>,
    /// The buffer of the last send, kept for the next one while more data
    /// waits.
    spare: Option<Vec<u8>>,
    closing: Option<CloseFuture>,
    state: WriteStatus,
}

#[derive(Debug)]
enum WriteStatus {
    Open,
    /// A send failed. `Some` until the error is reported once; the text is
    /// kept for the later ones.
    Failed(Option<io::Error>, String),
    /// `shutdown`/`close` finished.
    Closed,
}

impl<S: Transport> Compat<S> {
    /// Wraps `stream`, with a write buffer of [`DEFAULT_WRITE_BUFFER`]
    /// bytes.
    ///
    /// On a TCP connection (plain or under TLS) it also turns on
    /// `TCP_NODELAY`, as every giong crate does: `Compat` gathers small
    /// writes until a flush itself, and Nagle's algorithm on top of that
    /// only delays them, up to 40 ms per delayed acknowledgement (an HTTP/2
    /// test with 100 streams took 2.3 s instead of 21 ms). Undo it with
    /// `compat.get_ref().set_nodelay(false)` if you must.
    pub fn new(stream: S) -> Self {
        stream.no_delay();
        Self {
            stream: Rc::new(stream),
            read: ReadState {
                receiver: S::Receiver::default(),
                unread: Unread::None,
                eof: false,
            },
            write: WriteState {
                limit: DEFAULT_WRITE_BUFFER,
                buf: Vec::new(),
                sending: None,
                driver: None,
                spare: None,
                closing: None,
                state: WriteStatus::Open,
            },
        }
    }

    /// Sets how many written bytes `Compat` holds before a write has to wait
    /// for a send to finish (default [`DEFAULT_WRITE_BUFFER`], 64 KiB).
    ///
    /// While one buffer is sent the next one fills, so a busy stream holds
    /// up to twice `limit`. An idle one may keep one empty buffer of up to
    /// `limit` after a burst of writes (on the io_uring driver, where a
    /// send can still be running when the next write comes): give many
    /// mostly idle connections a smaller limit.
    ///
    /// # Panics
    ///
    /// With [`GIONG-E1403`](giong::error::Code::CompatConfig) if `limit` is
    /// 0 or more than [`MAX_WRITE_BUFFER`] (1 GiB).
    #[track_caller]
    pub fn write_buffer(mut self, limit: usize) -> Self {
        if limit == 0 || limit > MAX_WRITE_BUFFER {
            panic!(
                "{}",
                Error::new(
                    Code::CompatConfig,
                    format!("write_buffer({limit}) is out of range"),
                    "give a limit from 1 byte to 1 GiB (giong_compat::MAX_WRITE_BUFFER); the \
                     default, 64 KiB, suits most streams.",
                )
            );
        }
        self.write.limit = limit;
        self
    }

    /// The wrapped stream, for its own methods (`peer_addr`,
    /// `set_nodelay`, `alpn_protocol`, ...). Reading or writing it directly
    /// while `Compat` holds unread or unsent bytes mixes up the order of
    /// the data.
    pub fn get_ref(&self) -> &S {
        &self.stream
    }

    /// Sends everything written so far, then hands back the stream and the
    /// bytes received but not yet read, to carry on with giong's own API.
    ///
    /// A shutdown that has started is abandoned. Not cancel safe: if the
    /// future is dropped, the `Compat` is dropped with it.
    pub async fn into_inner(mut self) -> io::Result<(S, Vec<u8>)> {
        poll_fn(|cx| self.poll_flush_buffered(cx)).await?;
        let unread = self.read.unread.bytes().to_vec();
        let Compat {
            stream,
            read,
            write,
        } = self;
        // Whatever else holds the stream: a receive in progress (TLS, cancel
        // safe) and a shutdown in progress.
        drop(read);
        drop(write);
        match Rc::try_unwrap(stream) {
            Ok(stream) => Ok((stream, unread)),
            Err(_) => unreachable!("giong-compat: the stream is still in use after a flush"),
        }
    }

    // ------------------------------------------------------------- reading

    /// The bytes received and not yet read; empty at the end of the stream.
    fn poll_unread(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        while self.read.unread.is_empty() && !self.read.eof {
            match S::poll_chunk(&self.stream, &mut self.read.receiver, cx) {
                Poll::Pending => {
                    // About to wait: what was written before may be what the
                    // peer is waiting for.
                    self.send_for_read(cx);
                    return Poll::Pending;
                }
                Poll::Ready(Ok(chunk)) if chunk.as_slice().is_empty() => self.read.eof = true,
                Poll::Ready(Ok(chunk)) => self.read.unread = Unread::Chunk(chunk, 0),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            }
        }
        Poll::Ready(Ok(self.read.unread.bytes()))
    }

    /// Copies unread bytes into `dst`; 0 only at the end of the stream (or
    /// for an empty `dst`).
    #[cfg(feature = "futures-io")]
    fn poll_read_into(&mut self, cx: &mut Context<'_>, dst: &mut [u8]) -> Poll<io::Result<usize>> {
        if dst.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let src = ready!(self.poll_unread(cx))?;
        let n = src.len().min(dst.len());
        dst[..n].copy_from_slice(&src[..n]);
        self.read.unread.consume(n);
        Poll::Ready(Ok(n))
    }

    /// Hands buffered writes to the stream when a read is about to wait, and
    /// drives a send this task started (on the epoll driver a send only
    /// progresses when polled). A failure is kept for the next write.
    fn send_for_read(&mut self, cx: &mut Context<'_>) {
        let write = &self.write;
        if !matches!(write.state, WriteStatus::Open) {
            return;
        }
        let ours = match &write.driver {
            Some(driver) => driver.will_wake(cx.waker()),
            None => write.sending.is_none(),
        };
        if ours && (write.sending.is_some() || !write.buf.is_empty()) {
            let _ = self.poll_sent(cx);
        }
    }

    // ------------------------------------------------------------- writing

    /// The error a write, flush or shutdown returns, if it cannot go on.
    fn write_error(&mut self, writing: bool) -> Option<io::Error> {
        match &mut self.write.state {
            WriteStatus::Open => None,
            WriteStatus::Failed(first, text) => Some(first.take().unwrap_or_else(|| {
                Error::new(
                    Code::CompatWriteFailed,
                    format!("an earlier write on this stream failed ({text})"),
                    "the connection is broken: drop it and open a new one. Flush where you need \
                     to know that data was sent, so that a failure shows up there.",
                )
                .into()
            })),
            WriteStatus::Closed if writing => Some(
                Error::new(
                    Code::CompatShutDown,
                    "write after the stream was shut down",
                    "shut the stream down only after the last write; to send more, open a new \
                     connection.",
                )
                .into(),
            ),
            WriteStatus::Closed => None,
        }
    }

    /// Polls the send in progress; `Ready` once there is none. A failure is
    /// recorded in `state`, never returned.
    fn poll_send(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let write = &mut self.write;
        let Some(sending) = write.sending.as_mut() else {
            return Poll::Ready(());
        };
        match sending.as_mut().poll(cx) {
            Poll::Pending => {
                if !write
                    .driver
                    .as_ref()
                    .is_some_and(|d| d.will_wake(cx.waker()))
                {
                    write.driver = Some(cx.waker().clone());
                }
                Poll::Pending
            }
            Poll::Ready((res, mut buf)) => {
                write.sending = None;
                write.driver = None;
                match res {
                    Ok(()) if !write.buf.is_empty() => {
                        buf.clear();
                        write.spare = Some(buf);
                    }
                    Ok(()) => {}
                    Err(e) => {
                        let text = e.to_string();
                        write.state = WriteStatus::Failed(Some(e), text);
                        write.buf = Vec::new();
                        write.spare = None;
                    }
                }
                Poll::Ready(())
            }
        }
    }

    /// Starts sending the buffered bytes, if nothing is being sent.
    fn start_send(&mut self) {
        let write = &mut self.write;
        if write.sending.is_none() && !write.buf.is_empty() {
            let next = write.spare.take().unwrap_or_default();
            let buf = mem::replace(&mut write.buf, next);
            write.sending = Some(S::send(self.stream.clone(), buf));
        }
    }

    /// Sends until nothing is buffered or being sent, or a send fails;
    /// polling is what submits a send, so each one is polled once at once.
    fn poll_sent(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        loop {
            ready!(self.poll_send(cx));
            if self.write.buf.is_empty() || !matches!(self.write.state, WriteStatus::Open) {
                return Poll::Ready(());
            }
            self.start_send();
        }
    }

    fn poll_flush_buffered(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(e) = self.write_error(false) {
            return Poll::Ready(Err(e));
        }
        ready!(self.poll_sent(cx));
        Poll::Ready(match self.write_error(false) {
            Some(e) => Err(e),
            None => Ok(()),
        })
    }

    /// Accepts as much of `bufs` as fits, waiting for room if none does.
    fn poll_write_slices(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let len: usize = bufs.iter().map(|b| b.len()).sum();
        if let Some(e) = self.write_error(true) {
            return Poll::Ready(Err(e));
        }
        if len == 0 {
            return Poll::Ready(Ok(0));
        }
        // A finished send hands back its buffer (or its error) first.
        let _ = self.poll_send(cx);
        if self.write.buf.len() >= self.write.limit {
            // Full: send it, which waits for the send before it.
            self.start_send();
            let _ = self.poll_send(cx);
            if let Some(e) = self.write_error(true) {
                return Poll::Ready(Err(e));
            }
            if self.write.buf.len() >= self.write.limit {
                // The previous send is still going; `poll_send` registered
                // the waker.
                return Poll::Pending;
            }
            // The full buffer is on its way; the next one fills meanwhile.
        }
        if let Some(e) = self.write_error(true) {
            return Poll::Ready(Err(e));
        }
        let write = &mut self.write;
        let room = write.limit - write.buf.len();
        let n = len.min(room);
        if write.buf.capacity() - write.buf.len() < n {
            // Grow by doubling, but never past the limit.
            let want = (write.buf.len() + n)
                .max(write.buf.capacity() * 2)
                .min(write.limit);
            write.buf.reserve_exact(want - write.buf.len());
        }
        let mut left = n;
        for b in bufs {
            if left == 0 {
                break;
            }
            let take = b.len().min(left);
            write.buf.extend_from_slice(&b[..take]);
            left -= take;
        }
        Poll::Ready(Ok(n))
    }

    fn poll_write_bytes(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        self.poll_write_slices(cx, &[IoSlice::new(buf)])
    }

    fn poll_close_write(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            if let Some(closing) = self.write.closing.as_mut() {
                let res = ready!(closing.as_mut().poll(cx));
                self.write.closing = None;
                self.write.state = WriteStatus::Closed;
                return Poll::Ready(res);
            }
            match self.write.state {
                WriteStatus::Closed => return Poll::Ready(Ok(())),
                WriteStatus::Failed(..) => {
                    return Poll::Ready(Err(self.write_error(false).expect("failed")))
                }
                WriteStatus::Open => {}
            }
            ready!(self.poll_flush_buffered(cx))?;
            match S::close(&self.stream) {
                Ok(Some(closing)) => self.write.closing = Some(closing),
                Ok(None) => {
                    self.write.state = WriteStatus::Closed;
                    return Poll::Ready(Ok(()));
                }
                Err(e) => {
                    self.write.state = WriteStatus::Closed;
                    return Poll::Ready(Err(e));
                }
            }
        }
    }
}

impl Unread {
    fn bytes(&self) -> &[u8] {
        match self {
            Unread::None => &[],
            Unread::Chunk(chunk, at) => &chunk.as_slice()[*at..],
            Unread::Heap(buf, at) => &buf[*at..],
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Unread::None)
    }

    /// Marks `n` bytes as read. What is left of a pool buffer moves to the
    /// heap, so that the buffer goes back to the pool before the caller
    /// waits for anything else.
    fn consume(&mut self, n: usize) {
        match self {
            Unread::None => debug_assert_eq!(n, 0, "consume past the received bytes"),
            Unread::Chunk(chunk, at) => {
                let len = chunk.as_slice().len();
                *at = (*at + n).min(len);
                if *at == len {
                    *self = Unread::None;
                } else if matches!(chunk, Chunk::Pool(_)) {
                    *self = Unread::Heap(chunk.as_slice()[*at..].to_vec(), 0);
                }
            }
            Unread::Heap(buf, at) => {
                *at = (*at + n).min(buf.len());
                if *at == buf.len() {
                    *self = Unread::None;
                }
            }
        }
    }
}

impl<S: Transport> fmt::Debug for Compat<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match &self.write.state {
            WriteStatus::Open => "open",
            WriteStatus::Failed(..) => "failed",
            WriteStatus::Closed => "shut down",
        };
        f.debug_struct("Compat")
            .field("stream", &S::NAME)
            .field("write_buffer", &self.write.limit)
            .field("buffered", &self.write.buf.len())
            .field("sending", &self.write.sending.is_some())
            .field("write_state", &state)
            .field("unread", &self.read.unread.bytes().len())
            .field("eof", &self.read.eof)
            .finish()
    }
}

#[cfg(feature = "tokio")]
mod tokio_io {
    use super::*;
    use std::pin::Pin;
    use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};

    impl<S: Transport> AsyncRead for Compat<S> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if buf.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            let src = ready!(this.poll_unread(cx))?;
            let n = src.len().min(buf.remaining());
            buf.put_slice(&src[..n]);
            this.read.unread.consume(n);
            Poll::Ready(Ok(()))
        }
    }

    impl<S: Transport> AsyncBufRead for Compat<S> {
        fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
            self.get_mut().poll_unread(cx)
        }

        fn consume(self: Pin<&mut Self>, amt: usize) {
            self.get_mut().read.unread.consume(amt);
        }
    }

    impl<S: Transport> AsyncWrite for Compat<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().poll_write_bytes(cx, buf)
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().poll_write_slices(cx, bufs)
        }

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().poll_flush_buffered(cx)
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().poll_close_write(cx)
        }
    }
}

#[cfg(feature = "futures-io")]
mod futures_impl {
    use super::*;
    use futures_io::{AsyncBufRead, AsyncRead, AsyncWrite};
    use std::pin::Pin;

    impl<S: Transport> AsyncRead for Compat<S> {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().poll_read_into(cx, buf)
        }
    }

    impl<S: Transport> AsyncBufRead for Compat<S> {
        fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
            self.get_mut().poll_unread(cx)
        }

        fn consume(self: Pin<&mut Self>, amt: usize) {
            self.get_mut().read.unread.consume(amt);
        }
    }

    impl<S: Transport> AsyncWrite for Compat<S> {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().poll_write_bytes(cx, buf)
        }

        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().poll_write_slices(cx, bufs)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().poll_flush_buffered(cx)
        }

        fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().poll_close_write(cx)
        }
    }
}
