//! The accept loop and the per-connection state machine.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::hash_map::{Entry, HashMap};
use std::fmt;
use std::future::{poll_fn, Future};
use std::io::{self, Write as _};
use std::mem;
use std::net::{IpAddr, Shutdown, SocketAddr};
use std::ops::Deref;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::sync::OnceLock;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use giong::observe::RequestTrace;
use giong::time::{sleep, timeout, Alarm, Rang};

use crate::body::{Failure, Next, Pipe, RateClock, StreamBody};
use crate::budget::{self, Charge, FREE};
use crate::chunked::{Decoder, Progress};
use crate::h2::PREFACE;
use crate::heap::HeapFuture;
use crate::parse::{find_head_end, parse_head, Head};
use crate::request::{BodyBytes, Request};
use crate::response::{Body, Response};
use crate::trailers::Trailers;

/// Responses to pipelined requests are sent together, up to this many bytes.
const FLUSH_AT: usize = 64 * 1024;
/// A body at least this large is not copied after its head: the two go out
/// in one vectored write instead (see `Transport::write_all_with_body`).
const SEPARATE_BODY_AT: usize = 32 * 1024;

/// The rest of a request body of at least this many bytes is received
/// straight into the read buffer (`Transport::recv_into`), not copied out of
/// the runtime's receive buffers.
const RECV_INTO_AT: usize = 64 * 1024;

/// The room `recv_body_into` makes in the read buffer at a time: as much as
/// the buffer already holds, at least `FIRST_STEP` and at most `RESERVE_STEP`.
const FIRST_STEP: usize = 1024 * 1024;
const RESERVE_STEP: usize = 8 * 1024 * 1024;

/// After an error response, how long the connection keeps discarding input so
/// that the client can read the response instead of getting a reset.
const ERROR_LINGER: Duration = Duration::from_secs(1);
/// Blank lines before a request that still count as an idle connection.
const IDLE_BLANK_LINES: usize = 2;

/// Runs `f`; a panic comes back as `Err` instead of unwinding into the
/// connection. The panic hook has reported it by then.
pub(crate) fn catching<R>(f: impl FnOnce() -> R) -> Result<R, ()> {
    catch_unwind(AssertUnwindSafe(f)).map_err(drop_quietly)
}

/// Drops `value`, and swallows a panic of its destructor: a handler's
/// future that panicked once may panic again while it is dropped.
pub(crate) fn drop_quietly<T>(value: T) {
    let _ = catch_unwind(AssertUnwindSafe(move || drop(value)));
}

/// A handler's future whose panic becomes `Err`. A handler that panics
/// gets its request answered with `500` instead of taking down the
/// connection with it.
///
/// While requests are traced, it also carries the request's trace, and
/// polls the handler through it (see `giong::observe::RequestTrace`): one
/// branch per poll when they are not. It lives on the heap with the
/// handler's future, so that tracing does not make the connection's own
/// future larger; once the handler is done, the trace goes back through
/// `HANDED_BACK` (see `handed_back`).
struct Caught<F> {
    fut: F,
    trace: Option<Box<RequestTrace>>,
}

thread_local! {
    /// The trace of the handler that just finished, between `Caught`
    /// answering and its connection taking it back in the same poll.
    static HANDED_BACK: Cell<Option<Box<RequestTrace>>> = const { Cell::new(None) };
}

/// The trace of the handler `Caught` just finished, if requests are traced.
/// Called right after the handler's future answers, in the same poll, so
/// no other connection's handler can have finished meanwhile.
#[cold]
fn handed_back() -> Option<Box<RequestTrace>> {
    HANDED_BACK.with(Cell::take)
}

/// Empties `HANDED_BACK` before a traced handler starts. Every path that
/// finishes a traced `Caught` takes its trace back in the same poll, so the
/// slot is empty here; a path that forgot would otherwise give its stale
/// trace to the next traced request on this worker. A stale trace is
/// dropped and counted as abandoned. Only this crate's own tests panic on
/// it: a service's debug build (what `giong dev` runs) must not.
#[cold]
fn clear_handed_back() {
    let stale = handed_back();
    #[cfg(test)]
    assert!(
        stale.is_none(),
        "giong-http: a finished handler's request trace was not taken back in the poll that \
         finished it"
    );
    drop(stale);
}

impl<F: Future> Future for Caught<F> {
    type Output = Result<F::Output, ()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `fut` is pinned along with `self` (structural pinning):
        // it is never moved out, and `Caught` has no `Drop` of its own.
        // `trace` is not pinned.
        let this = unsafe { self.get_unchecked_mut() };
        let fut = unsafe { Pin::new_unchecked(&mut this.fut) };
        if this.trace.is_some() {
            return poll_traced(fut, &mut this.trace, cx);
        }
        match catching(|| fut.poll(cx)) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(()) => Poll::Ready(Err(())),
        }
    }
}

/// `Caught::poll` while requests are traced: polls the handler through its
/// trace, and hands the trace back once the handler is done.
#[cold]
#[inline(never)]
fn poll_traced<F: Future>(
    mut fut: Pin<&mut F>,
    trace: &mut Option<Box<RequestTrace>>,
    cx: &mut Context<'_>,
) -> Poll<Result<F::Output, ()>> {
    let polled = match trace {
        Some(t) => t.poll(|| catching(|| fut.as_mut().poll(cx))),
        None => catching(|| fut.as_mut().poll(cx)),
    };
    let answer = match polled {
        Ok(Poll::Pending) => return Poll::Pending,
        Ok(Poll::Ready(value)) => Ok(value),
        Err(()) => Err(()),
    };
    HANDED_BACK.set(trace.take());
    Poll::Ready(answer)
}

/// The answer to a request whose handler panicked.
pub(crate) fn panicked() -> Response {
    Response::new(500)
}

/// Capacity of a fresh connection buffer.
const BUFFER_SIZE: usize = 4 * 1024;
/// Largest buffer kept for reuse. A bigger one (grown for a large request or
/// response) is freed instead: handed to a small request, it would be held
/// for as long as that request's handler runs.
const POOL_MAX_BUFFER: usize = 16 * 1024;
/// Most bytes of buffers one worker keeps for reuse.
const POOL_BYTES: usize = 4 * 1024 * 1024;

/// Buffers recycled between the connections of one worker. A connection
/// holds none while it is idle, so keep-alive connections cost little.
struct Pool {
    buffers: Vec<Vec<u8>>,
    /// Capacity of all of `buffers`.
    bytes: usize,
}

thread_local! {
    static BUFFERS: RefCell<Pool> = const {
        RefCell::new(Pool {
            buffers: Vec::new(),
            bytes: 0,
        })
    };
}

pub(crate) fn take_buffer() -> Vec<u8> {
    BUFFERS
        .with(|pool| {
            let mut pool = pool.borrow_mut();
            let buf = pool.buffers.pop()?;
            pool.bytes -= buf.capacity();
            Some(buf)
        })
        .unwrap_or_else(|| Vec::with_capacity(BUFFER_SIZE))
}

pub(crate) fn give_buffer(mut buf: Vec<u8>) {
    buf.clear();
    let capacity = buf.capacity();
    if capacity == 0 || capacity > POOL_MAX_BUFFER {
        return;
    }
    BUFFERS.with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.bytes + capacity <= POOL_BYTES {
            pool.bytes += capacity;
            pool.buffers.push(buf);
        }
    });
}

/// A byte stream that HTTP can be served over: a [`TcpStream`], or a TLS
/// stream with the `tls` feature.
///
/// The methods follow [`TcpStream`]: buffers are passed by value and handed
/// back, and `recv` must be cancel safe (dropping its future loses no data),
/// because the server wraps it in timeouts.
pub trait Transport: 'static {
    /// What `recv` returns; released before the server awaits anything else.
    type Chunk: Deref<Target = [u8]>;

    /// Receives the next piece of data. Empty means end of stream.
    fn recv(&self) -> impl Future<Output = io::Result<Self::Chunk>>;

    /// Sends all of `buf` and hands it back.
    fn write_all(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<()>, Vec<u8>)>;

    /// Sends all of `head`, then all of `body`, and hands `head` back. The
    /// server uses it for large bodies.
    ///
    /// The default copies the body after the head and calls
    /// [`write_all`](Self::write_all). [`TcpStream`] and [`UnixStream`] send
    /// both with one vectored write instead, without the copy, and
    /// `giong_tls::TlsStream` hands both to TLS without the copy.
    fn write_all_with_body(
        &self,
        head: Vec<u8>,
        body: Body,
    ) -> impl Future<Output = (io::Result<()>, Vec<u8>)> {
        async move {
            let mut head = head;
            let len = head.len();
            head.extend_from_slice(&body.into_cow());
            let (res, mut head) = self.write_all(head).await;
            head.truncate(len);
            (res, head)
        }
    }

    /// Sends all of `head`, then `len` bytes of `file` from `offset`, and
    /// hands `head` back. The server uses it for [`Body::File`].
    ///
    /// The default sends the head, then reads the file in pieces of up to
    /// 256 KiB and writes each with [`write_all`](Self::write_all).
    /// [`TcpStream`] and `giong_tls::TlsStream` use their `send_file`
    /// instead, which over plain TCP and kernel TLS sends the file straight
    /// from the page cache. A file that ends before `offset + len` is an
    /// error (`GIONG-E0201`) after what it had was sent.
    fn write_all_with_file(
        &self,
        head: Vec<u8>,
        file: &giong::fs::File,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = (io::Result<()>, Vec<u8>)> {
        async move {
            let (res, mut head) = self.write_all(head).await;
            if res.is_err() {
                return (res, head);
            }
            let res = copy_file(file, offset, len, |piece| self.write_all(piece)).await;
            head.clear();
            (res, head)
        }
    }

    /// Closes the sending direction, so the peer sees the end of the stream
    /// while the server can still read what it sends.
    fn close_write(&self) -> impl Future<Output = ()>;

    /// Receives into the free capacity at the end of `buf` and hands it back
    /// with the number of bytes added; `Ok(0)` is the end of the stream. The
    /// server uses it for the rest of a large request body, whose size it
    /// knows. Not part of the API: it may change in any release.
    ///
    /// The default appends what [`recv`](Self::recv) returns.
    /// [`TcpStream`] and [`UnixStream`] receive straight into `buf`,
    /// without copying the body out of the runtime's receive buffers. Unlike
    /// `recv`, it need not be cancel safe: the server closes the connection
    /// when it gives up on one.
    #[doc(hidden)]
    fn recv_into(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        async move {
            let mut buf = buf;
            match self.recv().await {
                Ok(chunk) => {
                    buf.extend_from_slice(&chunk);
                    (Ok(chunk.len()), buf)
                }
                Err(e) => (Err(e), buf),
            }
        }
    }

    /// The address of the remote end, if the transport has one. Handlers see
    /// it as [`Request::peer_addr`]. The default is `None`.
    fn peer_addr(&self) -> Option<SocketAddr> {
        None
    }
}

impl Transport for TcpStream {
    type Chunk = giong::RecvBuf;

    fn recv(&self) -> impl Future<Output = io::Result<Self::Chunk>> {
        TcpStream::recv(self)
    }

    fn write_all(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<()>, Vec<u8>)> {
        TcpStream::write_all(self, buf)
    }

    fn recv_into(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        TcpStream::recv_into(self, buf)
    }

    async fn write_all_with_body(&self, head: Vec<u8>, body: Body) -> (io::Result<()>, Vec<u8>) {
        let bufs = vec![Cow::Owned(head), body.into_cow()];
        let (res, bufs) = TcpStream::write_all_vectored(self, bufs).await;
        (res, into_head(bufs))
    }

    async fn write_all_with_file(
        &self,
        head: Vec<u8>,
        file: &giong::fs::File,
        offset: u64,
        len: u64,
    ) -> (io::Result<()>, Vec<u8>) {
        let (res, head) = TcpStream::write_all(self, head).await;
        if res.is_err() {
            return (res, head);
        }
        (TcpStream::send_file(self, file, offset, len).await, head)
    }

    async fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        TcpStream::peer_addr(self).ok()
    }
}

impl Transport for UnixStream {
    type Chunk = giong::RecvBuf;

    fn recv(&self) -> impl Future<Output = io::Result<Self::Chunk>> {
        UnixStream::recv(self)
    }

    fn write_all(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<()>, Vec<u8>)> {
        UnixStream::write_all(self, buf)
    }

    fn recv_into(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        UnixStream::recv_into(self, buf)
    }

    async fn write_all_with_body(&self, head: Vec<u8>, body: Body) -> (io::Result<()>, Vec<u8>) {
        let bufs = vec![Cow::Owned(head), body.into_cow()];
        let (res, bufs) = UnixStream::write_all_vectored(self, bufs).await;
        (res, into_head(bufs))
    }

    async fn close_write(&self) {
        let _ = self.shutdown(Shutdown::Write);
    }
}

#[cfg(feature = "tls")]
impl Transport for giong_tls::TlsStream {
    type Chunk = giong_tls::Received;

    fn recv(&self) -> impl Future<Output = io::Result<Self::Chunk>> {
        giong_tls::TlsStream::recv(self)
    }

    fn write_all(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<()>, Vec<u8>)> {
        giong_tls::TlsStream::write_all(self, buf)
    }

    async fn write_all_with_body(&self, head: Vec<u8>, body: Body) -> (io::Result<()>, Vec<u8>) {
        let bufs = vec![Cow::Owned(head), body.into_cow()];
        let (res, bufs) = giong_tls::TlsStream::write_all_vectored(self, bufs).await;
        (res, into_head(bufs))
    }

    async fn write_all_with_file(
        &self,
        head: Vec<u8>,
        file: &giong::fs::File,
        offset: u64,
        len: u64,
    ) -> (io::Result<()>, Vec<u8>) {
        let (res, head) = giong_tls::TlsStream::write_all(self, head).await;
        if res.is_err() {
            return (res, head);
        }
        (
            giong_tls::TlsStream::send_file(self, file, offset, len).await,
            head,
        )
    }

    async fn close_write(&self) {
        let _ = self.shutdown().await;
    }

    fn peer_addr(&self) -> Option<SocketAddr> {
        self.tcp().peer_addr().ok()
    }
}

/// What answers requests: an `async fn(Request) -> Response`, a closure
/// returning a future, or a type of its own, such as a router.
///
/// Every function or closure `Fn(Request) -> impl Future<Output = Response>`
/// is a handler already; implement the trait for a type that needs to borrow
/// from itself while it answers.
///
/// ```
/// use giong_http::{Handler, Request, Response};
///
/// struct Greeter {
///     greeting: String,
/// }
///
/// impl Handler for Greeter {
///     async fn call(&self, req: Request) -> Response {
///         Response::text(format!("{}, {}", self.greeting, req.path()))
///     }
/// }
/// ```
pub trait Handler: 'static {
    /// Answers one request. The future may borrow from `self`; the server
    /// keeps the handler alive until every connection using it has closed.
    fn call(&self, req: Request) -> impl Future<Output = Response>;

    /// Whether to read the body of `req` as it arrives, and up to how many
    /// bytes.
    ///
    /// Called once the head of a request with a body has arrived, before
    /// any of the body is read ([`Request::body`] is empty then). `None`,
    /// the default, receives the whole body first, up to the server's
    /// [`max_body_size`](Server::max_body_size), and then calls the
    /// handler, as usual.
    ///
    /// `Some(limit)` calls the handler right away instead, with the body
    /// readable through [`Request::body_stream`] (see
    /// [`BodyStream`](crate::BodyStream)). A body announced as longer than
    /// `limit` gets `413 Content Too Large` without calling the handler; a
    /// chunked body that grows past it fails with `GIONG-E1111`. The
    /// server's `max_body_size` does not apply.
    ///
    /// A handler that answers before reading the body to its end makes the
    /// connection close after the response over HTTP/1.1; over HTTP/2 only
    /// its stream ends (the client is told to stop sending with
    /// `RST_STREAM`).
    fn stream_body(&self, req: &Request) -> Option<u64> {
        let _ = req;
        None
    }
}

/// A handler shared between servers: one `App` serving a plain port and a
/// TLS port, say, through `Rc::new(app)` and a clone for each. Both calls go
/// to the shared handler, [`stream_body`](Handler::stream_body) included.
impl<H: Handler> Handler for Rc<H> {
    fn call(&self, req: Request) -> impl Future<Output = Response> {
        (**self).call(req)
    }

    fn stream_body(&self, req: &Request) -> Option<u64> {
        (**self).stream_body(req)
    }
}

impl<F, Fut> Handler for F
where
    F: Fn(Request) -> Fut + 'static,
    Fut: Future<Output = Response>,
{
    fn call(&self, req: Request) -> impl Future<Output = Response> {
        self(req)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Config {
    pub(crate) idle_timeout: Duration,
    pub(crate) request_timeout: Duration,
    pub(crate) write_timeout: Duration,
    pub(crate) max_head_size: usize,
    pub(crate) max_body_size: usize,
    pub(crate) shutdown_linger: Duration,
    pub(crate) max_connections: usize,
    pub(crate) http2: bool,
    pub(crate) http2_prior_knowledge: bool,
    pub(crate) max_concurrent_streams: u32,
    /// The QPACK dynamic table HTTP/3 clients may fill, in bytes; 0 for
    /// none.
    pub(crate) qpack_table_size: u32,
    /// What an HTTP/2 connection may hold of request bodies and of
    /// responses not sent yet, instead of the larger of `max_body_size`
    /// and 1 MiB: for the fuzz harness, which needs a small one. `0` for
    /// none: an `Option` would take 8 bytes more in every connection.
    pub(crate) http2_budget: u64,
    pub(crate) max_buffered_request_bytes: BufferedLimit,
    pub(crate) min_body_rate: crate::body::MinRate,
}

thread_local! {
    /// The configuration the connections of this thread share (see
    /// [`shared_config`]).
    static SHARED_CONFIG: std::cell::RefCell<Option<Rc<Config>>> = const { std::cell::RefCell::new(None) };
}

/// One copy of `cfg` for every connection of this thread that has the same
/// configuration: a connection holds a pointer instead of the 120 bytes of
/// a copy of its own. A server is usually one per thread; one with other
/// settings replaces the copy kept here, and its connections share the new
/// one.
pub(crate) fn shared_config(cfg: &Config) -> Rc<Config> {
    SHARED_CONFIG.with(|shared| {
        let mut shared = shared.borrow_mut();
        match &*shared {
            Some(kept) if **kept == *cfg => kept.clone(),
            _ => {
                let kept = Rc::new(*cfg);
                *shared = Some(kept.clone());
                kept
            }
        }
    })
}

impl Config {
    /// The worker's budget for requests received in full (see
    /// `Server::max_buffered_request_bytes`). Worked out on the worker, the
    /// first time it is needed.
    pub(crate) fn buffered_limit(&self) -> u64 {
        match self.max_buffered_request_bytes {
            BufferedLimit::AUTO => {
                let single = (self.max_body_size as u64).saturating_add(self.max_head_size as u64);
                crate::budget::auto_limit(single)
            }
            BufferedLimit(limit) => limit,
        }
    }
}

/// `Server::max_buffered_request_bytes`: a number of bytes, or
/// [`AUTO`](Self::AUTO) for the default, which depends on the worker count
/// and the cgroup the process runs in and so is worked out on the worker.
/// Not an `Option`, which would take 8 bytes more in every connection.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct BufferedLimit(pub(crate) u64);

impl BufferedLimit {
    pub(crate) const AUTO: Self = Self(u64::MAX);

    fn bytes(bytes: usize) -> Self {
        // `u64::MAX` bytes is no limit anyway.
        Self((bytes as u64).min(u64::MAX - 1))
    }
}

impl fmt::Debug for BufferedLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::AUTO => {
                f.write_str("auto (64 MiB, or the cgroup memory limit / 4 / workers if less)")
            }
            Self(bytes) => write!(f, "{bytes}"),
        }
    }
}

/// An HTTP/1.1 and HTTP/2 server. Configure it, then call
/// [`serve`](Server::serve) from each worker of a giong runtime.
///
/// ```no_run
/// use std::time::Duration;
/// use giong::net::TcpListener;
/// use giong_http::{Request, Response, Server};
///
/// async fn hello(req: Request) -> Response {
///     match (req.method(), req.path()) {
///         ("GET", "/") => Response::text("hello"),
///         _ => Response::new(404),
///     }
/// }
///
/// fn main() -> std::io::Result<()> {
///     giong::Builder::new()
///         .shutdown_on_signals(true)
///         .shutdown_grace(Duration::from_secs(10))
///         .build()?
///         .run(|_worker| async {
///             let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
///             Server::new().serve(listener, hello).await;
///         })
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Server {
    pub(crate) cfg: Config,
    /// `min_free_memory`, or `u64::MAX` for the default. Kept out of
    /// `Config`, which every connection carries: only the accept loop needs
    /// it.
    min_free_memory: u64,
    /// `0` for none. Only the accept loop needs it.
    max_connections_per_ip: u32,
    /// `Duration::ZERO` for none. Only the accept loop needs it: a
    /// connection keeps the deadline it gives in its `deadline`.
    first_request_timeout: Duration,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// A server with the default limits and timeouts.
    pub fn new() -> Self {
        Self {
            cfg: Config {
                idle_timeout: Duration::from_secs(60),
                request_timeout: Duration::from_secs(30),
                write_timeout: Duration::from_secs(30),
                max_head_size: 64 * 1024,
                max_body_size: 1024 * 1024,
                shutdown_linger: Duration::from_millis(100),
                max_connections: 10_000,
                http2: true,
                http2_prior_knowledge: true,
                max_concurrent_streams: 100,
                qpack_table_size: 0,
                http2_budget: 0,
                max_buffered_request_bytes: BufferedLimit::AUTO,
                min_body_rate: crate::body::MinRate {
                    per_sec: 0,
                    grace_ms: 0,
                },
            },
            min_free_memory: u64::MAX,
            max_connections_per_ip: 0,
            first_request_timeout: Duration::ZERO,
        }
    }

    /// How long a keep-alive connection may sit between requests before it is
    /// closed (default: 60 s).
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.idle_timeout = crate::bounded(timeout);
        self
    }

    /// How long a client may take to send one complete request, counted from
    /// the moment the server first has to wait for more of it (default: 30 s).
    /// Slower clients get `408 Request Timeout`.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.request_timeout = crate::bounded(timeout);
        self
    }

    /// How long a client may go without taking any of what the server sends
    /// before the connection is dropped (default: 30 s). It counts from the
    /// last progress, so a large response to a slow client may take longer
    /// in all, as long as the client keeps reading.
    pub fn write_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.write_timeout = crate::bounded(timeout);
        self
    }

    /// Largest accepted request head, in bytes (default: 64 KiB). Larger heads
    /// get `431 Request Header Fields Too Large`.
    pub fn max_head_size(mut self, bytes: usize) -> Self {
        self.cfg.max_head_size = bytes.max(64);
        self
    }

    /// Largest accepted request body, in bytes (default: 1 MiB). Larger bodies
    /// get `413 Content Too Large`. A body read as a stream has the limit its
    /// handler gives instead (see [`Handler::stream_body`]).
    pub fn max_body_size(mut self, bytes: usize) -> Self {
        self.cfg.max_body_size = bytes;
        self
    }

    /// Most bytes of requests one worker holds while receiving them
    /// (default: 64 MiB, or less in a container: see below).
    ///
    /// [`max_head_size`](Self::max_head_size) and
    /// [`max_body_size`](Self::max_body_size) bound one request, and
    /// [`max_connections`](Self::max_connections) the number of
    /// connections, but not the sum: 10,000 clients each sending a 1 MiB
    /// body slowly would make one worker hold about 11 GiB. This bounds the
    /// sum. It counts what the server receives in full before calling the
    /// handler, over HTTP/1.1 and HTTP/2, on every connection of the worker
    /// (every `Server` serving on it shares the count, each against its own
    /// limit), from the moment the bytes arrive until the request is
    /// dropped. Over HTTP/1.1 the first 4 KiB of each request are not
    /// counted, so that ordinary requests keep being served while uploads
    /// fill the budget. A body read as a stream (see
    /// [`Handler::stream_body`]) is not counted: it holds one piece at a
    /// time.
    ///
    /// A request that would take the count past the limit is refused
    /// without calling the handler, with `503 Service Unavailable`,
    /// `Retry-After: 1` and `Connection: close` over HTTP/1.1 (before its
    /// body is read, if its `Content-Length` does not fit; otherwise as
    /// soon as the next bytes would not), and with `REFUSED_STREAM` over
    /// HTTP/2, which clients retry. The body of the `503` names
    /// `GIONG-E1120`, and each worker says on stderr, at most once a
    /// minute, how many requests it refused. A request larger than the
    /// limit itself gets `413 Content Too Large`.
    ///
    /// The default is 64 MiB per worker, or, when the process has a cgroup
    /// memory limit (a container's memory limit, systemd's `MemoryMax`), a
    /// quarter of that limit divided among the workers if that is less; but
    /// never less than one request of `max_head_size` plus `max_body_size`.
    /// With one worker in 128 MiB that is 32 MiB. `Debug` shows `auto` for
    /// it, since the worker count is only known on the worker.
    ///
    /// What a worker holds is then about this limit plus what its
    /// connections cost on their own (about 1.2 KiB each while idle over
    /// HTTP/1.1, 2.5 KiB over HTTP/2, 5.0 KiB over HTTPS and 6.2 KiB over
    /// HTTP/2 with TLS, and up to the 4 KiB above while a request arrives); see `docs/DEPLOYMENT.md` for
    /// sizing a container.
    pub fn max_buffered_request_bytes(mut self, bytes: usize) -> Self {
        self.cfg.max_buffered_request_bytes = BufferedLimit::bytes(bytes);
        self
    }

    /// The slowest average rate, in bytes per second, at which a request
    /// body read as a stream (see [`Handler::stream_body`]) may arrive once
    /// `grace` has passed (default: off).
    ///
    /// [`request_timeout`](Self::request_timeout) bounds the wait for each
    /// piece of a streamed body, not the whole body, so a client that sends
    /// one byte every 29 seconds holds a handler, its buffers and a
    /// connection (over HTTP/2 and HTTP/3, a stream) for as long as it
    /// likes: the slow-POST form of slowloris. With a minimum rate, the
    /// body may take the grace period plus the time its bytes earn at the
    /// rate, counting only the time the server waits for the client (not
    /// the time the handler takes with a piece, during which flow control
    /// holds the client back). A body that falls behind is cut: the
    /// handler's [`BodyStream::chunk`](crate::BodyStream::chunk) fails with
    /// `GIONG-E1112`, and the connection closes after the response (over
    /// HTTP/2 and HTTP/3, the stream is reset). The average is over the
    /// whole body, so a client that sent quickly at first may pause later.
    ///
    /// For uploads, 240 bytes per second after 10 seconds (what ASP.NET
    /// Core's Kestrel uses by default) cuts a trickle and no real client.
    /// Leave it off for long-lived streams that send little (a gRPC
    /// client stream of rare messages, a log tail), which
    /// `request_timeout` already bounds between two pieces. `0` turns it
    /// off; the grace is kept to whole milliseconds, at most about 49 days.
    pub fn min_body_rate(mut self, bytes_per_sec: u32, grace: Duration) -> Self {
        self.cfg.min_body_rate = crate::body::MinRate {
            per_sec: bytes_per_sec,
            grace_ms: u32::try_from(grace.as_millis()).unwrap_or(u32::MAX),
        };
        self
    }

    /// After a shutdown is requested, how long the server keeps picking up
    /// connections and requests that were already on their way (default:
    /// 100 ms). Requests in progress are not affected by this: they get the
    /// runtime's [`shutdown_grace`](giong::Builder::shutdown_grace).
    pub fn shutdown_linger(mut self, linger: Duration) -> Self {
        self.cfg.shutdown_linger = crate::bounded(linger);
        self
    }

    /// Most connections one [`serve`](Self::serve) call keeps open at once
    /// (default: 10,000). Normally every worker makes its own call, so the
    /// limit applies per worker.
    ///
    /// An idle connection costs a few KiB (about 1.2 KiB over HTTP/1.1, 2.5
    /// KiB over HTTP/2, 5.0 KiB over HTTPS, 6.2 KiB over HTTP/2 with TLS,
    /// plus the kernel's socket buffers), so the default holds a worker's idle connections to tens
    /// of MiB. The requests the connections are receiving have a bound of
    /// their own, shared by all of them:
    /// [`max_buffered_request_bytes`](Self::max_buffered_request_bytes).
    /// Raise this limit for many long-lived connections per worker (event
    /// streams), lower it to bound memory more tightly, as in a small
    /// container.
    ///
    /// At the limit, when a new client arrives, the connection that has
    /// been idle longest (HTTP/1.1 between requests, HTTP/2 with no request
    /// open; not an HTTP/1.1 connection still waiting for its first request,
    /// nor one whose next request has arrived) is closed to make room for it, as nginx does with keep-alive
    /// connections: idle connections, from one client or from many
    /// browsers, cannot keep new clients out. The idle connection ends as
    /// if its client had closed it (HTTP/2 sends `GOAWAY`), and clients
    /// open a new one when they need it. A request the client sent just as
    /// its connection was closed is lost, as with any keep-alive close, and
    /// clients retry it on a new connection only if it is idempotent (`GET`,
    /// `PUT`, `DELETE`...). A connection receiving or answering a request is
    /// never closed for this. If none is idle, the
    /// server stops accepting until a connection closes, and new clients
    /// wait in the kernel's listen queue instead of being refused, so the
    /// server does not run out of file descriptors because of the number
    /// of clients (one client past the limit is accepted and waits). Idle
    /// connections are closed this way on Linux and macOS; on Windows the
    /// server waits. Connections handed to
    /// [`serve_connection`](Self::serve_connection) are not counted, and
    /// HTTP/3 connections are not closed this way.
    ///
    /// Below the limit, new connections are refused while the process is
    /// low on memory: see [`min_free_memory`](Self::min_free_memory).
    pub fn max_connections(mut self, max: usize) -> Self {
        self.cfg.max_connections = max.max(1);
        self
    }

    /// Most connections one client address may have open at once on one
    /// [`serve`](Self::serve) call (default: no limit). Normally every
    /// worker makes its own call, so the limit applies per worker, as
    /// [`max_connections`](Self::max_connections) does.
    ///
    /// [`max_connections`](Self::max_connections) keeps the connections of
    /// a worker within what it can hold, but one client may take all of
    /// them: connections that never send a request are not idle (their
    /// client would not retry a request lost on them), so they are not
    /// closed to make room, and each holds its place until
    /// [`idle_timeout`](Self::idle_timeout), or
    /// [`first_request_timeout`](Self::first_request_timeout) if sooner.
    /// With this limit, a client that has `max` connections open has its
    /// next one refused at once, before an idle connection is asked to
    /// make room for it: plain HTTP/1.1 clients get `429 Too Many Requests`
    /// with `Retry-After: 1`, `Connection: close` and `GIONG-E1124` in the
    /// body, TLS clients a closed connection (before the handshake, which
    /// is what costs). The other clients keep being served. Each worker
    /// says on stderr, at most once a minute, how many connections it
    /// refused and the last address refused.
    ///
    /// An IPv4 address counts as one client, and so does an IPv6 /64
    /// network, the block one subscriber is usually given (an IPv4 address
    /// mapped into IPv6 counts as that IPv4 address). Connections on a Unix
    /// socket ([`serve_unix`](Self::serve_unix)) and HTTP/3 connections are
    /// not counted, nor those handed to
    /// [`serve_connection`](Self::serve_connection).
    ///
    /// Behind a load balancer or a reverse proxy, every connection comes
    /// from the proxy's few addresses: leave this off there (the proxy is
    /// where to limit clients), or set it above what the proxy opens. Many
    /// people may also share one IPv4 address (carrier-grade NAT, an
    /// office), so a public server should allow some hundreds: a browser
    /// opens up to 6 HTTP/1.1 connections to a host, and one over HTTP/2.
    /// `0` turns the limit off. With it on, the accept loop looks up each
    /// new connection's address (one `getpeername` call) and keeps a count
    /// per address that has connections open.
    pub fn max_connections_per_ip(mut self, max: usize) -> Self {
        self.max_connections_per_ip = u32::try_from(max).unwrap_or(u32::MAX);
        self
    }

    /// How long a new connection may take, from the moment it is accepted,
    /// to send the head of its first request (default: off, so the first
    /// request waits like any other: [`idle_timeout`](Self::idle_timeout)
    /// for its first byte, then [`request_timeout`](Self::request_timeout)).
    ///
    /// A connection that has sent no request holds a place under
    /// [`max_connections`](Self::max_connections) that is never given to a
    /// new client (see there), for 60 seconds by default and up to
    /// [`request_timeout`](Self::request_timeout) more if it trickles a
    /// head: the cheapest way for a crowd of clients to keep a server's
    /// places taken. With a deadline of a few seconds each such connection
    /// holds its place that long instead. Over TLS
    /// ([`serve_tls`](Self::serve_tls)) the deadline counts from before the
    /// handshake, which has its own limit (giong-tls's
    /// `TlsAcceptor::handshake_timeout`, 10 seconds by default). A
    /// connection that has sent nothing by then is closed; one in the
    /// middle of a head gets `408 Request Timeout`. The body is not
    /// counted: once the head is in, it has
    /// [`request_timeout`](Self::request_timeout) as usual. HTTP/2 with
    /// prior knowledge must have sent its preface by then; HTTP/2 chosen
    /// through ALPN, and later requests on a connection, are not affected.
    ///
    /// Clients send their first request as soon as they connect; a
    /// browser that opened a connection ahead of need (a preconnect) may
    /// hold it unused for a while, and opens another if it was closed. Go's
    /// `ReadHeaderTimeout` and ASP.NET Core's `RequestHeadersTimeout` (30
    /// seconds by default) are the same limit. `Duration::ZERO` turns it
    /// off. It applies to [`serve`](Self::serve),
    /// [`serve_unix`](Self::serve_unix), [`serve_tls`](Self::serve_tls) and
    /// [`serve_connection`](Self::serve_connection).
    pub fn first_request_timeout(mut self, timeout: Duration) -> Self {
        self.first_request_timeout = crate::bounded(timeout);
        self
    }

    /// Memory to keep free: while less than this is left to the process,
    /// new connections are refused (default: a tenth of the memory limit,
    /// between 16 and 256 MiB; see below). `0` turns the check off.
    ///
    /// The other limits bound what each connection and each request may
    /// hold, but not what the process holds in all: a worker's
    /// [`max_connections`](Self::max_connections) idle HTTPS connections
    /// take about 170 MiB with the kernel's socket memory, and more workers
    /// or a higher limit take more. Past the memory the process may use,
    /// the kernel's out-of-memory killer ends it, with every connection it
    /// holds. Instead, the accept loop checks what is left before it takes
    /// on a connection, and while less than this is left, it accepts and
    /// closes new connections at once: plain HTTP/1.1 clients get `503
    /// Service Unavailable` with `Retry-After: 1` and `GIONG-E1121` in the
    /// body, TLS clients a closed connection. The connections the server
    /// has keep being served, and new ones are taken on again as soon as
    /// memory is freed. Each worker says on stderr, at most once a minute,
    /// how many connections it refused.
    ///
    /// What is left is the smaller of what the process's cgroup allows (a
    /// container's memory limit or systemd's `MemoryMax`: `memory.max`, or
    /// `memory.high` if lower, less what the cgroup uses apart from the
    /// page cache the kernel can reclaim) and what the machine has
    /// (`MemAvailable` in `/proc/meminfo`; on macOS, which has no cgroups,
    /// the free and inactive pages the kernel reports). A worker reads it at
    /// most every 50 ms while far from the limit and on every accept close
    /// to it.
    ///
    /// The default is a tenth of the cgroup's memory limit, or of the
    /// machine's memory without one, but at least 16 MiB, at most 256 MiB,
    /// and never more than a quarter of the memory: 51 MiB in a container
    /// of 512 MiB. Raise it if the program allocates large amounts outside
    /// of its connections (caches, a database client's buffers), so that
    /// those allocations still find room when connections have filled the
    /// rest.
    pub fn min_free_memory(mut self, bytes: usize) -> Self {
        self.min_free_memory = (bytes as u64).min(u64::MAX - 1);
        self
    }

    /// Whether to speak HTTP/2 (default: yes).
    ///
    /// Over TLS ([`serve_tls`](Self::serve_tls)) the server offers `h2` and
    /// `http/1.1` through ALPN, and each client picks; browsers pick `h2`.
    /// Without TLS, a connection that starts with the HTTP/2 preface is
    /// served over HTTP/2 ("prior knowledge", as gRPC clients and proxies
    /// such as Envoy speak to backends), and any other over HTTP/1.1; see
    /// [`http2_prior_knowledge`](Self::http2_prior_knowledge) to keep plain
    /// connections on HTTP/1.1. Handlers are the same either way:
    /// [`Request::version`] tells them apart.
    ///
    /// `false` serves HTTP/1.1 only, and offers only `http/1.1` through
    /// ALPN.
    ///
    /// [`Request::version`]: crate::Request::version
    pub fn http2(mut self, enabled: bool) -> Self {
        self.cfg.http2 = enabled;
        self
    }

    /// Whether a connection without TLS that starts with the HTTP/2
    /// preface is served over HTTP/2 (default: yes, when
    /// [`http2`](Self::http2) is on).
    ///
    /// `false` keeps HTTP/2 to TLS connections that choose it through ALPN:
    /// for a server whose plain port sits behind a proxy that speaks
    /// HTTP/1.1 only, so that nothing but HTTP/1.1 is spoken there. A
    /// connection that starts with the preface then gets `505 HTTP Version
    /// Not Supported`, as with `http2(false)`.
    pub fn http2_prior_knowledge(mut self, enabled: bool) -> Self {
        self.cfg.http2_prior_knowledge = enabled;
        self
    }

    /// Most requests one HTTP/2 connection may have in progress at once
    /// (default: 100, the least RFC 9113 recommends). Further streams are
    /// refused with `REFUSED_STREAM`, and the client retries them later.
    ///
    /// Memory does not grow with it as it would with as many connections.
    /// Request bodies: the bodies one connection holds share a budget of
    /// the larger of [`max_body_size`](Self::max_body_size) and 1 MiB. Each
    /// stream may send its share of the budget (half the budget divided by
    /// this limit, at least 4 KiB and at most 64 KiB) before the server
    /// grants more; the server grants more while the budget allows, after
    /// setting aside the shares of streams that may still open, and always to
    /// the oldest stream still receiving, so that past the budget bodies are
    /// received in turn instead of together. A connection therefore holds
    /// at most the budget and 256 KiB more, or this limit times 4 KiB if
    /// that is larger.
    ///
    /// Responses: handlers run while the response bytes a connection holds
    /// (answered, not sent yet) are within the same budget. A client that
    /// does not take what it asked for keeps the other handlers of its
    /// connection waiting, as an HTTP/1.1 client that does not read keeps
    /// its next request waiting, so a connection holds at most the budget
    /// and the answer that went past it. A `&'static` body does not count,
    /// and a file only for the 64 KiB piece of it being sent.
    pub fn max_concurrent_streams(mut self, max: u32) -> Self {
        self.cfg.max_concurrent_streams = max.max(1);
        self
    }

    /// Accepts connections on `listener` and answers their requests with
    /// `handler` until the runtime is asked to shut down.
    ///
    /// Each connection runs as its own task on the current worker. When a
    /// shutdown is requested, the listener stops accepting, idle connections
    /// are closed, and requests being handled are finished and answered with
    /// `Connection: close` (over HTTP/2: a GOAWAY, after which the streams
    /// in progress finish), within the grace period configured on the
    /// runtime.
    ///
    /// A handler that panics has its request answered with `500 Internal
    /// Server Error` (the panic hook reports the panic as usual, on the
    /// worker: with `RUST_BACKTRACE` set, the standard hook symbolizes a
    /// backtrace there, and every connection on the worker waits for it;
    /// see `docs/BEST_PRACTICES.md`). Over
    /// HTTP/1.1 its connection then closes; over HTTP/2 only its stream
    /// ends, and the other requests on the connection go on. A handler
    /// whose answer had begun when it panicked (a streamed body's producer
    /// runs elsewhere, so this is rare) has its stream reset instead.
    ///
    /// Cancel safe: if the future is dropped, the listener is closed;
    /// connections already accepted go on as their own tasks.
    pub async fn serve<H: Handler>(&self, listener: TcpListener, handler: H) {
        let handler = Rc::new(handler);
        let cfg = shared_config(&self.cfg);
        let first = self.first_request_timeout;
        let accept = || listener.accept();
        self.accept_loop(
            accept,
            fd_of,
            tcp_peer_ip,
            |stream: TcpStream, slot| {
                let _ = stream.set_nodelay(true);
                drop(giong::spawn(
                    Conn::new(stream, cfg.clone(), Some(slot))
                        .first_request_by(first_deadline(first))
                        .serve(handler.clone()),
                ));
            },
            refuse,
        )
        .await;
    }

    /// Like [`serve`](Self::serve), on a Unix domain socket: the usual way to
    /// sit behind a reverse proxy on the same machine.
    ///
    /// ```no_run
    /// # use giong::net::UnixListener;
    /// # use giong_http::{Request, Response, Server};
    /// # async fn handler(_: Request) -> Response { Response::new(200) }
    /// # async fn demo() -> std::io::Result<()> {
    /// let listener = UnixListener::bind("/run/app.sock")?;
    /// Server::new().serve_unix(listener, handler).await;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Cancel safe, like [`serve`](Self::serve).
    pub async fn serve_unix<H: Handler>(&self, listener: UnixListener, handler: H) {
        let handler = Rc::new(handler);
        let cfg = shared_config(&self.cfg);
        let first = self.first_request_timeout;
        let accept = || listener.accept();
        self.accept_loop(
            accept,
            fd_of,
            |_: &UnixStream| None,
            |stream: UnixStream, slot| {
                drop(giong::spawn(
                    Conn::new(stream, cfg.clone(), Some(slot))
                        .first_request_by(first_deadline(first))
                        .serve(handler.clone()),
                ));
            },
            refuse,
        )
        .await;
    }

    /// Like [`serve`](Self::serve), over TLS: every accepted connection first
    /// completes a TLS handshake with `acceptor`.
    ///
    /// ```no_run
    /// # use giong::net::TcpListener;
    /// # use giong_http::{Request, Response, Server};
    /// # async fn handler(_: Request) -> Response { Response::new(200) }
    /// # async fn demo() -> std::io::Result<()> {
    /// let acceptor = giong_tls::TlsAcceptor::with_certificates(&giong_tls::Certificates::from_pem(
    ///     &std::fs::read("cert.pem")?,
    ///     &std::fs::read("key.pem")?,
    /// )?)?;
    /// let listener = TcpListener::bind("0.0.0.0:8443")?;
    /// Server::new().serve_tls(listener, acceptor, handler).await;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Cancel safe, like [`serve`](Self::serve).
    #[cfg(feature = "tls")]
    pub async fn serve_tls<H: Handler>(
        &self,
        listener: TcpListener,
        acceptor: giong_tls::TlsAcceptor,
        handler: H,
    ) {
        let handler = Rc::new(handler);
        let cfg = shared_config(&self.cfg);
        // The server says which protocols it speaks, whatever the acceptor
        // was configured with.
        let acceptor = if cfg.http2 {
            acceptor.alpn(&[b"h2", b"http/1.1"])
        } else {
            acceptor.alpn(&[b"http/1.1"])
        };
        let first = self.first_request_timeout;
        let accept = || listener.accept();
        self.accept_loop(
            accept,
            fd_of,
            tcp_peer_ip,
            |stream: TcpStream, slot| {
                let _ = stream.set_nodelay(true);
                let acceptor = acceptor.clone();
                let handler = handler.clone();
                let cfg = cfg.clone();
                // Counted from the accept, so the handshake is within it.
                let first = first_deadline(first);
                // The handshake, on the heap for its length only, holding what
                // the connection needs (the acceptor and TCP stream for the
                // handshake, then the rest), and reading the connection's address
                // and protocol. The connection's task then holds the TLS stream
                // once, inside the HTTP state, and nothing else: an input the
                // task captures, or a local of it that is borrowed, keeps its
                // bytes in the task for as long as the connection lasts.
                let handshake = HeapFuture::with(move || async move {
                    let tls = acceptor.accept(stream).await?;
                    let h2 = tls.alpn_protocol().as_deref() == Some(b"h2");
                    let peer = Transport::peer_addr(&tls);
                    io::Result::Ok((tls, h2, peer, cfg, slot, handler, first))
                });
                drop(giong::spawn(async move {
                    // A failed handshake (a port scan, a plain-HTTP client, an
                    // unknown protocol version) just ends the connection.
                    let Ok((tls, h2, peer, cfg, slot, handler, first)) = handshake.await else {
                        return;
                    };
                    if h2 {
                        // A task of its own, and this one ends: this task is
                        // as large as the HTTP/1.1 state, which an HTTP/2
                        // connection has no use for, and HTTP/1.1
                        // connections do not carry the HTTP/2 state.
                        drop(giong::spawn(serve_h2(
                            tls,
                            cfg,
                            peer,
                            handler,
                            slot,
                            Vec::new,
                        )));
                    } else {
                        Conn::new(tls, cfg, Some(slot))
                            .first_request_by(first)
                            .serve(handler)
                            .await;
                    }
                }));
            },
            // Nothing can be said before the handshake, which is what
            // costs memory: just close.
            |stream, _| drop(stream),
        )
        .await;
    }

    /// The dynamic table an HTTP/3 client's QPACK encoder may fill, in
    /// bytes (default: 0, none). Needs the `unstable-http3` feature.
    ///
    /// With a table, a client sends a field it repeats (a cookie, its
    /// `user-agent`, the `:authority`) once and refers to it after, so
    /// requests take fewer bytes and the server decodes fewer literals. A
    /// connection holds at most this many bytes of table, counted as RFC
    /// 9204 counts them (each field's name and value and 32 bytes), and is
    /// charged for it only once the client inserts something. At most 16
    /// requests of a connection may wait for insertions at once; one more
    /// is a connection error, as RFC 9204 says, and so is a client that
    /// leaves more than 16 KiB of the server's acknowledgments unread
    /// (`H3_EXCESSIVE_LOAD`).
    ///
    /// Responses still use the static table alone.
    #[cfg(feature = "unstable-http3")]
    pub fn qpack_table_size(mut self, bytes: u32) -> Self {
        self.cfg.qpack_table_size = bytes;
        self
    }

    /// Serves HTTP/3 (RFC 9114) on a QUIC endpoint until a shutdown is
    /// requested, with the same handler and the same limits as the other
    /// `serve` methods. Needs the `unstable-http3` feature, and may change
    /// until HTTP/3 is stabilised.
    ///
    /// The endpoint's TLS configuration must name the ALPN protocol `h3`.
    /// Bind one endpoint on every worker, each with its own call of this
    /// method, as with a TCP listener.
    ///
    /// What differs from HTTP/2, because QUIC decides it: the number of
    /// requests a client may have open at once is the endpoint's
    /// `TransportConfig::max_streams` (not `max_concurrent_streams`), and an
    /// idle connection is closed after the endpoint's
    /// `TransportConfig::idle_timeout` (not `idle_timeout`). On shutdown the
    /// client is told with `GOAWAY`, requests in flight are finished, and
    /// later ones are refused with `H3_REQUEST_REJECTED`, which lets the
    /// client retry them elsewhere.
    ///
    /// Responses a client does not take have the bound they have over
    /// HTTP/2 (see [`max_concurrent_streams`](Self::max_concurrent_streams)):
    /// handlers run while the response bytes a connection holds are within
    /// the larger of `max_body_size` and 1 MiB, so that a client asking for
    /// many large answers and giving no flow-control credit holds at most
    /// that, the answer that went past it, and what QUIC itself buffers (its
    /// connection window, 8 MiB by default).
    ///
    /// Clients find HTTP/3 through the `Alt-Svc` header field of responses
    /// over TCP (RFC 7838): have the server that answers on TCP add it
    /// with `Response::header("alt-svc", "h3=\":443\"; ma=86400")`, with
    /// the UDP port this endpoint is bound to.
    ///
    /// ```no_run
    /// # use giong_http::{Request, Response, Server};
    /// # fn tls() -> rustls::ServerConfig { unimplemented!() }
    /// # async fn handler(_: Request) -> Response { Response::new(200) }
    /// # async fn demo() -> std::io::Result<()> {
    /// // `tls()` is a rustls configuration with `alpn_protocols` set to
    /// // `vec![b"h3".to_vec()]`.
    /// let config = giong_quic::ServerConfig::new(tls())?;
    /// let endpoint = giong_quic::Endpoint::server("0.0.0.0:443".parse().unwrap(), config)?;
    /// Server::new().serve_h3(endpoint, handler).await;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Cancel safe, like [`serve`](Self::serve): dropping the future stops
    /// accepting; connections already accepted are served to their end.
    #[cfg(feature = "unstable-http3")]
    pub async fn serve_h3<H: Handler>(&self, endpoint: giong_quic::Endpoint, handler: H) {
        let handler = Rc::new(handler);
        let cfg = self.cfg;
        let accept = || async {
            endpoint.accept().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotConnected, "the QUIC endpoint is closed")
            })
        };
        self.accept_loop(
            accept,
            // QUIC connections share the endpoint's socket.
            |_| -1,
            // Not counted by `max_connections_per_ip`.
            |_| None,
            |conn: giong_quic::Connection, slot| {
                let handler = handler.clone();
                drop(giong::spawn(async move {
                    let _slot = slot;
                    crate::h3::server::serve(conn, cfg, handler).await;
                }));
            },
            // Memory is low: H3_EXCESSIVE_LOAD (RFC 9114, section 8.1).
            |conn: giong_quic::Connection, _| conn.close(0x107, b""),
        )
        .await;
    }

    /// Accepts until a shutdown is requested, then for a moment longer.
    /// Waits for a free slot under `max_connections` before each accept, and
    /// hands a connection to `refuse` instead of `spawn` while memory is low
    /// (`min_free_memory`) or when its client's address has
    /// `max_connections_per_ip` open (`peer_ip` gives the address, and is
    /// only called with that limit on).
    async fn accept_loop<S, A>(
        &self,
        accept: impl Fn() -> A,
        fd: impl Fn(&S) -> i32,
        peer_ip: impl Fn(&S) -> Option<IpAddr>,
        spawn: impl Fn(S, Slot),
        refuse: impl Fn(S, &'static [u8]),
    ) where
        A: Future<Output = io::Result<S>>,
    {
        let reserve = match self.min_free_memory {
            u64::MAX => crate::memory::auto_reserve(),
            bytes => bytes,
        };
        let max_per_ip = self.max_connections_per_ip;
        // `None` for a connection refused before it had a slot.
        let spawn = |stream: S, slot: Option<Slot>, ip: Option<IpAddr>| {
            let Some(mut slot) = slot else {
                refuse(stream, TOO_MANY_FROM_ADDRESS);
                note_refused_address(max_per_ip, ip);
                return;
            };
            slot.set_fd(fd(&stream));
            if crate::memory::low(reserve) {
                drop(slot);
                refuse(stream, LOW_MEMORY);
                note_refused_connection(reserve);
            } else if !slot.count_ip(ip) {
                drop(slot);
                refuse(stream, TOO_MANY_FROM_ADDRESS);
                note_refused_address(max_per_ip, ip);
            } else {
                spawn(stream, slot);
            }
        };
        let slots = Rc::new(Slots::new(self.cfg.max_connections, max_per_ip));
        // The client's address, only for `max_connections_per_ip`.
        let ip_of = |stream: &S| {
            if max_per_ip == 0 {
                None
            } else {
                peer_ip(stream)
            }
        };
        open_files_limits();
        // Below the limit, a slot first and then the connection. At the
        // limit, the connection first, so that an idle connection is asked
        // to make room only for a client that is there (one connection past
        // the limit waits meanwhile, instead of in the listen queue), and
        // not for one past its address's share, which is refused first.
        //
        // Cancel safe: a slot taken by a dropped future is given back, and
        // `accept` itself loses no connection, except the one accepted at
        // the limit while it waits for its slot.
        let next = || async {
            if slots.full() {
                let stream = accept().await?;
                let ip = ip_of(&stream);
                if slots.address_full(ip) {
                    return Ok((stream, None, ip));
                }
                Ok((stream, Some(slots.take(true).await), ip))
            } else {
                let slot = slots.take(false).await;
                accept().await.map(|stream| {
                    let ip = ip_of(&stream);
                    (stream, Some(slot), ip)
                })
            }
        };
        while let Some(accepted) = giong::until_shutdown(next()).await {
            match accepted {
                Ok((stream, slot, ip)) => spawn(stream, slot, ip),
                // For example EMFILE: back off instead of spinning.
                Err(e) => {
                    if matches!(e.raw_os_error(), Some(EMFILE | ENFILE)) {
                        note_out_of_files(&e);
                    }
                    sleep(Duration::from_millis(50)).await
                }
            }
        }
        // Connections established just before the shutdown still get served.
        let deadline = Instant::now() + self.cfg.shutdown_linger;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match timeout(left, next()).await {
                Ok(Ok((stream, slot, ip))) => spawn(stream, slot, ip),
                _ => break,
            }
        }
    }

    /// Serves a single, already established connection until it is closed.
    /// For servers that run their own accept loop or their own transport.
    pub fn serve_connection<T, H>(&self, stream: T, handler: Rc<H>) -> impl Future<Output = ()>
    where
        T: Transport,
        H: Handler,
    {
        Conn::new(stream, shared_config(&self.cfg), None)
            .first_request_by(first_deadline(self.first_request_timeout))
            .serve(handler)
    }
}

/// The open connections of one accept loop, against its limit. Only the
/// accept loop waits for a slot, so one waker is enough.
///
/// At the limit, with a new client waiting, an idle connection (HTTP/1.1
/// between requests, HTTP/2 with no stream open) is asked to close, so that
/// idle keep-alive connections cannot keep new clients out, as nginx reuses
/// its keep-alive connections. The accept loop asks by shutting down the
/// reading side of the connection's socket: the connection reads the end
/// of its stream, as if the client had closed it, and closes as it would
/// then. So no connection keeps a waker for this, and the accept loop keeps
/// three bits and two numbers per connection, in places numbered from 0
/// and reused: memory follows the connections this loop had open at once,
/// not the file descriptor numbers of the process. A connection marks
/// itself idle and busy (an HTTP/1.1 connection only after its first
/// request); the search for the one idle longest runs only at the limit,
/// and passes over a connection whose next request has arrived. Only on Unix: elsewhere connections are not marked, and the
/// accept loop waits for one to close.
struct Slots {
    open: Cell<usize>,
    max: usize,
    waiter: Cell<Option<Waker>>,
    /// Per place: the connection's socket, and when it last became idle.
    places: RefCell<Vec<Place>>,
    /// A bit per place: a connection is in it.
    used: RefCell<Vec<u64>>,
    /// A bit per place: the connection in it is idle.
    idle: RefCell<Vec<u64>>,
    /// A bit per place: the connection in it was asked to close.
    evicted: RefCell<Vec<u64>>,
    /// No word of `used` below this one has a free place.
    free_from: Cell<usize>,
    next_idle: Cell<u32>,
    /// Connections asked to close that have not closed yet.
    evicting: Cell<usize>,
    /// `max_connections_per_ip`, when it is on.
    per_ip: Option<PerIp>,
}

/// The counts behind `Server::max_connections_per_ip`, in [`Slots`].
struct PerIp {
    max: u32,
    /// Connections open by client, under [`ip_key`]; a client with none
    /// has no entry. `HashMap`'s keyed hash, since clients choose their
    /// addresses.
    open: RefCell<HashMap<u64, u32>>,
    /// Per place: the client its connection is counted under.
    keys: RefCell<Vec<Option<u64>>>,
}

/// The client a connection is counted under by `max_connections_per_ip`:
/// its IPv4 address, or the /64 network of its IPv6 address, the block one
/// subscriber is usually given (an IPv4 address mapped into IPv6 is taken
/// as that IPv4 address).
fn ip_key(ip: IpAddr) -> u64 {
    // IPv4 keys look like IPv6 networks in ff00::/8 (multicast), which no
    // TCP client has.
    const V4: u64 = 0xffff_ffff_0000_0000;
    match ip {
        IpAddr::V4(v4) => V4 | u64::from(u32::from(v4)),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => V4 | u64::from(u32::from(v4)),
            None => (u128::from(v6) >> 64) as u64,
        },
    }
}

/// One connection's entry in [`Slots`].
#[derive(Clone, Copy)]
struct Place {
    #[cfg_attr(not(unix), allow(dead_code))]
    fd: i32,
    /// When the connection last became idle, in `next_idle` order
    /// (wrapping).
    since: u32,
}

/// A [`Slot`] with no place: the accept loop does not shut its connection
/// down (QUIC, or not on Unix).
const NO_PLACE: u32 = u32::MAX;

impl Slots {
    fn new(max: usize, max_per_ip: u32) -> Self {
        Self {
            open: Cell::new(0),
            max,
            waiter: Cell::new(None),
            places: RefCell::new(Vec::new()),
            used: RefCell::new(Vec::new()),
            idle: RefCell::new(Vec::new()),
            evicted: RefCell::new(Vec::new()),
            free_from: Cell::new(0),
            next_idle: Cell::new(0),
            evicting: Cell::new(0),
            per_ip: (max_per_ip > 0).then(|| PerIp {
                max: max_per_ip,
                open: RefCell::new(HashMap::new()),
                keys: RefCell::new(Vec::new()),
            }),
        }
    }

    fn full(&self) -> bool {
        self.open.get() >= self.max
    }

    /// The client at `ip` has `max_connections_per_ip` connections open.
    fn address_full(&self, ip: Option<IpAddr>) -> bool {
        match (&self.per_ip, ip) {
            (Some(per_ip), Some(ip)) => per_ip
                .open
                .borrow()
                .get(&ip_key(ip))
                .is_some_and(|&n| n >= per_ip.max),
            _ => false,
        }
    }

    /// Waits until fewer than `max` connections are open, then takes a slot.
    /// Meanwhile, with `evict` (a new connection waits for the slot), asks
    /// an idle connection to close, unless one asked is still closing.
    async fn take(self: &Rc<Self>, evict: bool) -> Slot {
        poll_fn(|cx| {
            if !self.full() {
                self.open.set(self.open.get() + 1);
                Poll::Ready(Slot {
                    slots: self.clone(),
                    place: NO_PLACE,
                    ready: Cell::new(false),
                })
            } else {
                self.waiter.set(Some(cx.waker().clone()));
                if evict && self.evicting.get() == 0 {
                    self.evict_one();
                }
                Poll::Pending
            }
        })
        .await
    }

    /// Asks the connection idle longest to close, if one is idle.
    ///
    /// A connection whose next request has already arrived is passed over
    /// (it is about to be busy): shutting the reading side of a socket down
    /// discards what it holds on macOS and the BSDs, where Linux keeps it
    /// readable, and the request would be lost.
    #[cfg(unix)]
    fn evict_one(&self) {
        let mut idle = self.idle.borrow_mut();
        let places = self.places.borrow();
        let now = self.next_idle.get();
        let place = loop {
            let mut oldest: Option<(u32, usize)> = None;
            for (word, &bits) in idle.iter().enumerate() {
                let mut bits = bits;
                while bits != 0 {
                    let place = word * 64 + bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    let age = now.wrapping_sub(places[place].since);
                    if oldest.map_or(true, |(a, _)| age > a) {
                        oldest = Some((age, place));
                    }
                }
            }
            let Some((_, place)) = oldest else {
                return;
            };
            idle[place / 64] &= !(1u64 << (place % 64));
            if !has_input(places[place].fd) {
                break place;
            }
        };
        let (word, bit) = (place / 64, 1u64 << (place % 64));
        let mut evicted = self.evicted.borrow_mut();
        if evicted[word] & bit == 0 {
            evicted[word] |= bit;
            self.evicting.set(self.evicting.get() + 1);
        }
        // SAFETY: `fd` is the socket of an open connection of this loop,
        // which keeps it open until its slot is dropped (right after it
        // closes the socket, in the same poll), and `ManuallyDrop` never
        // closes it here.
        let socket = mem::ManuallyDrop::new(unsafe {
            <std::net::TcpStream as std::os::fd::FromRawFd>::from_raw_fd(
                places[place].fd as std::os::fd::RawFd,
            )
        });
        // `shutdown` is the same call for TCP and Unix sockets. It fails
        // only if the client is already gone, and the connection then reads
        // the end of its stream anyway.
        let _ = socket.shutdown(std::net::Shutdown::Read);
    }

    #[cfg(not(unix))]
    fn evict_one(&self) {}
}

/// One open connection; frees its slot when dropped.
pub(crate) struct Slot {
    slots: Rc<Slots>,
    /// The connection's place in [`Slots`], or [`NO_PLACE`].
    place: u32,
    /// The connection may be marked idle: it has received something (see
    /// [`set_idle`](Self::set_idle)), or is HTTP/2. In the padding after
    /// `place`.
    ready: Cell<bool>,
}

impl Slot {
    /// Records the connection's socket, accepted after the slot was taken,
    /// in the lowest free place.
    fn set_fd(&mut self, fd: i32) {
        let slots = &*self.slots;
        // No socket to shut down: QUIC, which is not counted per address
        // either, or a connection not on Unix, which gets a place only to
        // be counted by `max_connections_per_ip` (`evict_one` does nothing
        // there).
        if fd < 0 && (cfg!(unix) || slots.per_ip.is_none()) {
            return;
        }
        let mut used = slots.used.borrow_mut();
        let mut word = slots.free_from.get();
        while word < used.len() && used[word] == u64::MAX {
            word += 1;
        }
        if word == used.len() {
            used.push(0);
            slots.idle.borrow_mut().push(0);
            slots.evicted.borrow_mut().push(0);
        }
        let bit = (!used[word]).trailing_zeros() as usize;
        used[word] |= 1 << bit;
        slots.free_from.set(word);
        let place = word * 64 + bit;
        let mut places = slots.places.borrow_mut();
        // Every place below this one is taken, so it is at most one past
        // the end.
        let entry = Place { fd, since: 0 };
        match places.get_mut(place) {
            Some(p) => *p = entry,
            None => places.push(entry),
        }
        self.place = place as u32;
    }

    /// The connection waits for its next request (`true`) or is serving
    /// one (`false`). Becoming idle while the accept loop waits at the limit
    /// wakes it, to ask this connection or another idle one to close.
    ///
    /// A connection is marked idle only once it has been busy (it received
    /// something), or after [`allow_idle`](Self::allow_idle): a new HTTP/1.1
    /// connection waiting for its first request is not idle, as its client
    /// would not retry a request lost on it (clients retry only on a
    /// connection they have used before).
    pub(crate) fn set_idle(&self, idle: bool) {
        if !idle {
            self.ready.set(true);
        }
        if self.place == NO_PLACE || (idle && !self.ready.get()) {
            return;
        }
        let place = self.place as usize;
        let (word, bit) = (place / 64, 1u64 << (place % 64));
        let slots = &*self.slots;
        let mut bits = slots.idle.borrow_mut();
        if !idle {
            bits[word] &= !bit;
        } else if bits[word] & bit == 0 {
            // A connection asked to close may come back here once (if a
            // request was on its way), then reads the end of its stream:
            // nothing else is asked to close meanwhile (`evicting`).
            bits[word] |= bit;
            let order = slots.next_idle.get();
            slots.next_idle.set(order.wrapping_add(1));
            slots.places.borrow_mut()[place].since = order;
            if slots.full() {
                if let Some(waker) = slots.waiter.take() {
                    waker.wake();
                }
            }
        }
    }
}

impl Slot {
    /// Lets the connection be marked idle before it receives anything: an
    /// HTTP/2 connection closes with a `GOAWAY` that tells its client which
    /// requests were not served, and so may be retried.
    pub(crate) fn allow_idle(&self) {
        self.ready.set(true);
    }

    /// Counts the connection under its client's address for
    /// `max_connections_per_ip`, unless the client has that many open
    /// already: then `false`. Always `true` with the limit off, or without
    /// an address (a Unix socket, or one that could not be read).
    fn count_ip(&self, ip: Option<IpAddr>) -> bool {
        let (Some(per_ip), Some(ip)) = (&self.slots.per_ip, ip) else {
            return true;
        };
        if self.place == NO_PLACE {
            return true;
        }
        let key = ip_key(ip);
        let mut open = per_ip.open.borrow_mut();
        let n = open.entry(key).or_insert(0);
        if *n >= per_ip.max {
            return false;
        }
        *n += 1;
        let place = self.place as usize;
        let mut keys = per_ip.keys.borrow_mut();
        if keys.len() <= place {
            keys.resize(place + 1, None);
        }
        keys[place] = Some(key);
        true
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let slots = &*self.slots;
        if self.place != NO_PLACE {
            let place = self.place as usize;
            if let Some(per_ip) = &slots.per_ip {
                let key = per_ip
                    .keys
                    .borrow_mut()
                    .get_mut(place)
                    .and_then(Option::take);
                if let Some(key) = key {
                    if let Entry::Occupied(mut n) = per_ip.open.borrow_mut().entry(key) {
                        *n.get_mut() -= 1;
                        if *n.get() == 0 {
                            n.remove();
                        }
                    }
                }
            }
            let (word, bit) = (place / 64, 1u64 << (place % 64));
            slots.used.borrow_mut()[word] &= !bit;
            slots.idle.borrow_mut()[word] &= !bit;
            let mut evicted = slots.evicted.borrow_mut();
            if evicted[word] & bit != 0 {
                evicted[word] &= !bit;
                slots.evicting.set(slots.evicting.get() - 1);
            }
            if word < slots.free_from.get() {
                slots.free_from.set(word);
            }
        }
        slots.open.set(slots.open.get() - 1);
        if let Some(waker) = slots.waiter.take() {
            waker.wake();
        }
    }
}

/// Whether bytes wait to be read on `fd`, without taking them and without
/// blocking (sockets accepted by io_uring are blocking).
#[cfg(unix)]
fn has_input(fd: i32) -> bool {
    let mut byte = 0u8;
    // SAFETY: `fd` is an open socket (see `Slots::evict_one`), and one byte
    // of `byte` is writable. `MSG_PEEK` leaves the byte in the socket.
    let n = unsafe {
        libc::recv(
            fd,
            (&mut byte as *mut u8).cast(),
            1,
            libc::MSG_PEEK | libc::MSG_DONTWAIT,
        )
    };
    n > 0
}

/// The socket of a connection, for [`Slots`].
#[cfg(unix)]
fn fd_of<S: std::os::fd::AsRawFd>(stream: &S) -> i32 {
    stream.as_raw_fd()
}

#[cfg(not(unix))]
fn fd_of<S>(_: &S) -> i32 {
    -1
}

/// Runs `write` until it completes, or until it has made no progress for
/// `idle`: then `None`, and the write is dropped.
///
/// A transport's write only completes, so its progress is seen from outside:
/// a write future that returns `Pending` waits for the socket, and is polled
/// again when the socket took bytes (a send completed, or the socket became
/// writable). So every poll after the first that the timer did not cause is
/// progress. Something else waking the connection's task counts as progress
/// too, which can only make the wait longer, by `idle` at most for each
/// such wake. A write done at its first poll arms no timer.
///
/// A future of its own, not an `async fn`: it lives in every connection's
/// future, and an `async fn` would keep the write future twice.
fn until_stalled<F: Future>(idle: &Duration, write: F) -> UntilStalled<'_, F> {
    UntilStalled {
        write,
        idle,
        timer: None,
    }
}

struct UntilStalled<'a, F> {
    /// Pinned with the `UntilStalled`: never moved out.
    write: F,
    /// Borrowed: one word less in every connection's future.
    idle: &'a Duration,
    /// From the first wait on: runs out `idle` after the write last moved.
    timer: Option<giong::time::Sleep>,
}

impl<F: Future> Future for UntilStalled<'_, F> {
    type Output = Option<F::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: `write` is structurally pinned: it is only reached through
        // the pinned reference made here and never moved. The other fields
        // are `Unpin`.
        let this = unsafe { self.get_unchecked_mut() };
        // SAFETY: `write` is structurally pinned (see above).
        let write = unsafe { Pin::new_unchecked(&mut this.write) };
        if let Poll::Ready(done) = write.poll(cx) {
            return Poll::Ready(Some(done));
        }
        let now = Instant::now();
        match &mut this.timer {
            None => {
                // The write waits for the first time: count from now.
                let timer = this
                    .timer
                    .insert(giong::time::sleep_until(now + *this.idle));
                if Pin::new(timer).poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
            }
            Some(timer) => {
                if now < timer.deadline() {
                    // Woken before the timer: the write moved. Counting
                    // from now re-arms the timer, which costs only a write
                    // that does not complete at once.
                    timer.reset(now + *this.idle);
                }
                if Pin::new(timer).poll(cx).is_ready() {
                    return Poll::Ready(None);
                }
            }
        }
        Poll::Pending
    }
}

/// Refused connections a worker may still be answering at once; past this
/// they are closed without an answer, so that refusing costs no memory.
const MAX_REFUSING: usize = 64;

thread_local! {
    /// Refused connections being answered on this worker.
    static REFUSING: Cell<usize> = const { Cell::new(0) };
    /// Connections refused since the worker last said so, and when it did.
    static REFUSED: Cell<(u64, Option<Instant>)> = const { Cell::new((0, None)) };
    /// Accepts that failed for want of file descriptors since the worker
    /// last said so, and when it did.
    static OUT_OF_FILES: Cell<(u64, Option<Instant>)> = const { Cell::new((0, None)) };
    /// Connections refused under `max_connections_per_ip` since the worker
    /// last said so, and when it did.
    static REFUSED_ADDRESS: Cell<(u64, Option<Instant>)> = const { Cell::new((0, None)) };
}

/// `accept` errors for want of file descriptors: the process's limit, and
/// the system's (the same numbers on Linux and macOS).
const EMFILE: i32 = 24;
const ENFILE: i32 = 23;

/// The answer to a connection refused because memory is low.
const LOW_MEMORY: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\n\
    retry-after: 1\r\n\
    connection: close\r\n\
    content-type: text/plain; charset=utf-8\r\n\
    content-length: 102\r\n\
    \r\n\
    GIONG-E1121: the server is low on memory and takes no new connections \
    (min_free_memory). Retry later.\n";

/// The answer to a connection refused because its client's address has
/// `max_connections_per_ip` connections open.
const TOO_MANY_FROM_ADDRESS: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\n\
    retry-after: 1\r\n\
    connection: close\r\n\
    content-type: text/plain; charset=utf-8\r\n\
    content-length: 115\r\n\
    \r\n\
    GIONG-E1124: this address has as many connections open as the server allows \
    (max_connections_per_ip). Retry later.\n";

/// Refuses a connection without TLS (low memory, or its address over
/// `max_connections_per_ip`): sends `answer`, then closes, in a small task
/// of its own that holds no read buffer until the answer is out.
fn refuse<T: Transport>(stream: T, answer: &'static [u8]) {
    let busy = REFUSING.with(|n| {
        if n.get() >= MAX_REFUSING {
            return true;
        }
        n.set(n.get() + 1);
        false
    });
    if busy {
        return;
    }
    drop(giong::spawn(async move {
        struct Done;
        impl Drop for Done {
            fn drop(&mut self) {
                let _ = REFUSING.try_with(|n| n.set(n.get() - 1));
            }
        }
        let _done = Done;
        let answer = async {
            let (res, _) = stream.write_all(answer.to_vec()).await;
            if res.is_err() {
                return;
            }
            // Close the sending side and read what the client sends until it
            // closes too, so that the client reads the answer instead of a
            // reset for unread data.
            stream.close_write().await;
            while let Ok(chunk) = stream.recv().await {
                if chunk.is_empty() {
                    break;
                }
            }
        };
        let _ = timeout(ERROR_LINGER, answer).await;
    }));
}

/// Notes a connection refused for low memory; at most once a minute per
/// worker, says so on stderr, with the code and how many were refused.
fn note_refused_connection(reserve: u64) {
    let _ = REFUSED.try_with(|refused| {
        let (count, last) = refused.get();
        let now = Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(60)) {
            refused.set((count + 1, last));
            return;
        }
        refused.set((0, Some(now)));
        eprintln!(
            "giong-http: GIONG-E1121 refused {} connection(s) on this worker: less than {} MiB \
             of memory is left to the process, its min_free_memory; see docs/errors/E1121.md",
            count + 1,
            reserve >> 20
        );
    });
}

/// Notes a connection refused under `max_connections_per_ip`; at most once
/// a minute per worker, says so on stderr, with the code, how many were
/// refused and the last address.
fn note_refused_address(max: u32, ip: Option<IpAddr>) {
    let _ = REFUSED_ADDRESS.try_with(|refused| {
        let (count, last) = refused.get();
        let now = Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(60)) {
            refused.set((count + 1, last));
            return;
        }
        refused.set((0, Some(now)));
        let from = ip
            .map(|ip| format!(", the last from {ip}"))
            .unwrap_or_default();
        eprintln!(
            "giong-http: GIONG-E1124 refused {} connection(s) on this worker from addresses that \
             had {max} open, its max_connections_per_ip{from}; see docs/errors/E1124.md",
            count + 1,
        );
    });
}

/// The client's address of a TCP connection, for `max_connections_per_ip`.
fn tcp_peer_ip(stream: &TcpStream) -> Option<IpAddr> {
    stream.peer_addr().ok().map(|addr| addr.ip())
}

/// When a connection accepted now must have sent its first request's head,
/// under `Server::first_request_timeout` (`ZERO` for none).
fn first_deadline(timeout: Duration) -> Option<Instant> {
    if timeout.is_zero() {
        None
    } else {
        Some(Instant::now() + timeout)
    }
}

/// Says on stderr, at most once a minute per worker, that connections wait
/// because the process is out of file descriptors (GIONG-E1123): without it
/// a server at its limit serves the connections it has and says nothing,
/// while new clients wait in the listen queue.
fn note_out_of_files(e: &io::Error) {
    let _ = OUT_OF_FILES.try_with(|failed| {
        let (count, last) = failed.get();
        let now = Instant::now();
        if last.is_some_and(|last| now.duration_since(last) < Duration::from_secs(60)) {
            failed.set((count + 1, last));
            return;
        }
        failed.set((0, Some(now)));
        let limits = open_files_limits()
            .map(|(soft, hard)| format!(" (open files: {soft}, hard limit {hard})"))
            .unwrap_or_default();
        eprintln!(
            "giong-http: GIONG-E1123 could not accept {} connection(s) on this worker: {e}{limits}; \
             new clients wait in the listen queue until a connection closes. Raise the hard \
             limit on open files (docker run --ulimit nofile=, LimitNOFILE= in systemd) or \
             lower max_connections; see docs/errors/E1123.md",
            count + 1,
        );
    });
}

/// The soft and hard limits on open files, from `/proc/self/limits`, read
/// once when the first accept loop starts: reading them needs a descriptor,
/// which is what is missing when they are wanted.
fn open_files_limits() -> Option<&'static (String, String)> {
    static LIMITS: OnceLock<Option<(String, String)>> = OnceLock::new();
    LIMITS
        .get_or_init(|| {
            let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
            let line = limits.lines().find(|l| l.starts_with("Max open files"))?;
            let mut fields = line.split_whitespace().skip(3);
            Some((fields.next()?.to_owned(), fields.next()?.to_owned()))
        })
        .as_ref()
}

/// An HTTP/2 connection of an accept loop, chosen through ALPN or by its
/// preface: its own task body. `input` gives what has been received
/// already: `Vec::new` after ALPN, which takes no room in the task.
///
/// Not an `async fn`: one keeps its arguments twice (as received, and moved
/// into its body), and the stream, a TLS session, is over a kilobyte.
#[allow(clippy::manual_async_fn)]
fn serve_h2<T: Transport, H: Handler>(
    stream: T,
    cfg: Rc<Config>,
    peer: Option<SocketAddr>,
    handler: Rc<H>,
    slot: Slot,
    input: impl FnOnce() -> Vec<u8>,
) -> impl Future<Output = ()> {
    async move {
        crate::h2::serve(&stream, &cfg, &peer, &*handler, input(), Some(&slot)).await;
        // Closed before the slot gives the connection's place back.
        drop(stream);
    }
}

/// What is left to receive of a request's body.
enum Wait {
    /// The rest of a chunked body, decoded so far by this decoder.
    Chunked(Decoder),
    /// Until the read buffer holds this many bytes of body.
    Length(usize),
}

/// How [`Conn::recv_until`] ended.
enum Waited<C> {
    Received(io::Result<C>),
    TimedOut,
}

/// Why a connection stops reading requests.
enum End {
    /// Close without a response: end of stream, idle timeout, I/O error.
    Close,
    /// Answer with this error status, then close.
    Status(u16),
    /// The connection starts with the HTTP/2 preface: hand it over.
    Http2,
    /// The worker holds as many request bytes as it may
    /// (`max_buffered_request_bytes`): answer `503`, then close.
    Busy,
}

/// The answer to a request refused because the worker holds as many
/// request bytes as it may.
fn busy() -> Response {
    Response::new(503)
        .header("retry-after", "1")
        .header("content-type", "text/plain; charset=utf-8")
        .body(
            &b"GIONG-E1120: the server is receiving as many requests as its memory budget \
               allows (max_buffered_request_bytes). Retry later.\n"[..],
        )
}

/// A request body read as it arrives (see `Handler::stream_body`).
struct Streamed {
    pipe: Rc<Pipe>,
    chunked: bool,
    /// The announced length, for a body that is not chunked.
    length: u64,
    limit: u64,
    /// The client waits for `100 Continue` before sending the body.
    expect_continue: bool,
}

/// Appends `n` in hexadecimal, as a chunk size.
fn push_hex(out: &mut Vec<u8>, n: usize) {
    let _ = write!(out, "{n:x}");
}

/// Reads `len` bytes of `file` from `offset` in pieces of up to 256 KiB and
/// hands each to `write`: [`Transport::write_all_with_file`] for transports
/// that cannot send from the file itself.
async fn copy_file<W, F>(file: &giong::fs::File, offset: u64, len: u64, write: W) -> io::Result<()>
where
    W: Fn(Vec<u8>) -> F,
    F: Future<Output = (io::Result<()>, Vec<u8>)>,
{
    const PIECE: u64 = 256 * 1024;
    let mut buf = Vec::with_capacity(len.min(PIECE) as usize);
    let (mut at, mut left) = (offset, len);
    while left > 0 {
        let (res, mut b) = file.read_at(buf, at).await;
        let n = match res {
            Ok(0) => {
                return Err(giong::error::Error::new(
                    giong::error::Code::FileTooShort,
                    format!(
                        "the file ended at byte {at}, {left} bytes short of the {len} the \
                         response announced"
                    ),
                    "take the length from the open file (Body::file does), and replace a \
                     file that is being served by renaming a new one over it, not by \
                     rewriting it.",
                )
                .into())
            }
            Ok(n) => (n as u64).min(left),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                buf = b;
                continue;
            }
            Err(e) => return Err(e),
        };
        b.truncate(n as usize);
        let (res, back) = write(b).await;
        res?;
        buf = back;
        at += n;
        left -= n;
    }
    Ok(())
}

/// The head buffer back from a vectored write of `[head, body]`.
fn into_head(mut bufs: Vec<Cow<'static, [u8]>>) -> Vec<u8> {
    match bufs.swap_remove(0) {
        Cow::Owned(head) => head,
        Cow::Borrowed(_) => Vec::new(),
    }
}

struct Conn<T> {
    stream: T,
    /// Shared by the connections of the thread (see [`shared_config`]).
    cfg: Rc<Config>,
    /// The client's address, looked up once per connection.
    peer: Option<SocketAddr>,
    /// Received bytes. Shared with the requests handed to the handler, so it
    /// is only modified in place once those have been dropped.
    buf: Rc<Vec<u8>>,
    /// Where the unparsed data starts in `buf`.
    pos: usize,
    /// How much of the unparsed data has been searched for the end of a head.
    /// While a large body is awaited (`body_into`), how many bytes from `pos`
    /// the read buffer must hold instead: no head is searched for then.
    scanned: usize,
    /// Responses not sent yet.
    out: Vec<u8>,
    /// A large body to send right after `out`, which ends with its head.
    body: Body,
    /// The server is shutting down: answer what arrives, then close.
    closing: bool,
    /// Deadline for the request currently being received. Set before the
    /// first request under `Server::first_request_timeout`, and then the
    /// only time it is set while the connection waits with nothing
    /// received (see `read_more`).
    deadline: Option<Instant>,
    /// `deadline` is `first_request_timeout`'s, which ends with the first
    /// head: the body then has `request_timeout`.
    head_deadline: bool,
    /// The timeout of the wait for a request, and the shutdown signal, armed
    /// once for the connection rather than once per wait (see `recv_until`).
    alarm: Alarm,
    /// Nothing has been parsed yet: the connection may still turn out to be
    /// HTTP/2 with prior knowledge.
    first: bool,
    /// A large body is awaited: `read_more` receives it straight into the
    /// read buffer, until the buffer holds `scanned` bytes from `pos`. (A
    /// flag beside the others, and a field that is idle meanwhile, so that
    /// no connection grows.)
    body_into: bool,
    /// What the unparsed data and the body being decoded hold against the
    /// worker's budget for requests (see `budget`).
    charge: Charge,
    /// Where the requests followed while request tracing is on (see
    /// `giong::observe::RequestTrace`) are kept, in the worker's `TRACES`:
    /// its index plus one, 0 for none. Kept there and not in the connection,
    /// so that tracing does not make idle connections larger (this fits in
    /// what the connection's layout leaves free).
    traces: TracesAt,
    /// The connection's place under `max_connections`, given back when the
    /// connection is dropped. Declared last, so that the stream is closed
    /// first.
    _slot: Option<Slot>,
}

/// See `Conn::traces`.
#[derive(Default)]
struct Traces {
    /// The request being received or answered. Its handler's wrapper
    /// (`Caught`) holds it while the handler runs.
    current: Option<Box<RequestTrace>>,
    /// Requests answered whose answers are in `out`, finished once it is
    /// written.
    answered: Vec<RequestTrace>,
}

thread_local! {
    /// The worker's `Traces`, by connection (see `Conn::traces`); `None`
    /// marks a free entry, listed in `free`.
    static TRACES: RefCell<TraceSlab> = const {
        RefCell::new(TraceSlab {
            entries: Vec::new(),
            free: Vec::new(),
        })
    };
}

struct TraceSlab {
    entries: Vec<Option<Traces>>,
    free: Vec<u32>,
}

/// A new entry in `TRACES`, as `Conn::traces` names it.
fn new_traces() -> u32 {
    TRACES
        .try_with(|slab| {
            let mut slab = slab.borrow_mut();
            if let Some(i) = slab.free.pop() {
                slab.entries[i as usize] = Some(Traces::default());
                return i + 1;
            }
            let Ok(i) = u32::try_from(slab.entries.len()) else {
                return 0;
            };
            if i == u32::MAX {
                return 0;
            }
            slab.entries.push(Some(Traces::default()));
            i + 1
        })
        .unwrap_or(0)
}

/// Runs `f` on the entry `slot` of `TRACES` (not 0).
fn with_traces<R>(slot: u32, f: impl FnOnce(&mut Traces) -> R) -> Option<R> {
    TRACES
        .try_with(|slab| {
            let mut slab = slab.borrow_mut();
            slab.entries
                .get_mut(slot as usize - 1)
                .and_then(Option::as_mut)
                .map(f)
        })
        .ok()
        .flatten()
}

/// Frees the entry `slot` of `TRACES` (not 0). The requests it still
/// follows are dropped, after the slab is released: they count as
/// abandoned.
fn free_traces(slot: u32) {
    let traces = TRACES
        .try_with(|slab| {
            let mut slab = slab.borrow_mut();
            let traces = slab
                .entries
                .get_mut(slot as usize - 1)
                .and_then(Option::take);
            if traces.is_some() {
                slab.free.push(slot - 1);
            }
            traces
        })
        .ok()
        .flatten();
    drop(traces);
}

/// See `Conn::traces`: where a connection's traces are kept in `TRACES`,
/// its index plus one, 0 for none. The entry is freed with this, and not by
/// a `Drop` of the connection, so that a connection handed over to HTTP/2
/// can be taken apart (see `Conn::serve`).
struct TracesAt(u32);

impl Drop for TracesAt {
    fn drop(&mut self) {
        if self.0 != 0 {
            free_traces(self.0);
        }
    }
}

impl<T: Transport> Conn<T> {
    fn new(stream: T, cfg: Rc<Config>, slot: Option<Slot>) -> Self {
        Self {
            peer: stream.peer_addr(),
            stream,
            cfg,
            buf: Rc::new(Vec::new()),
            pos: 0,
            scanned: 0,
            out: Vec::new(),
            body: Body::Empty,
            closing: false,
            deadline: None,
            head_deadline: false,
            alarm: Alarm::new(),
            first: true,
            body_into: false,
            charge: Charge::none(),
            traces: TracesAt(0),
            _slot: slot,
        }
    }

    /// Gives the first request until `deadline` for its head
    /// (`Server::first_request_timeout`).
    fn first_request_by(mut self, deadline: Option<Instant>) -> Self {
        self.head_deadline = deadline.is_some();
        self.deadline = deadline;
        self
    }

    /// Starts following a request whose first bytes are in, if it is not
    /// followed yet. Called when `RequestTrace::is_on`.
    #[cold]
    #[inline(never)]
    fn trace_arrival(&mut self) {
        if self.traces.0 != 0 && with_traces(self.traces.0, |t| t.current.is_some()) == Some(true) {
            return;
        }
        if let Some(trace) = RequestTrace::start("http/1.1") {
            if self.traces.0 == 0 {
                self.traces.0 = new_traces();
            }
            if self.traces.0 != 0 {
                with_traces(self.traces.0, |t| t.current = Some(Box::new(trace)));
            }
        }
    }

    /// The trace of the request whose handler is about to be called, taken
    /// out for the handler's wrapper to hold.
    #[cold]
    #[inline(never)]
    fn trace_handler(&mut self) -> Option<Box<RequestTrace>> {
        if self.traces.0 == 0 {
            return None;
        }
        clear_handed_back();
        let mut trace = with_traces(self.traces.0, |t| t.current.take()).flatten()?;
        trace.started();
        Some(trace)
    }

    /// The handler is done with the trace `trace_handler` took.
    #[cold]
    #[inline(never)]
    fn trace_back(&mut self, trace: Option<Box<RequestTrace>>) {
        if let (true, Some(trace)) = (self.traces.0 != 0, trace) {
            with_traces(self.traces.0, |t| t.current = Some(trace));
        }
    }

    /// The handler answered with `status`: the answer goes out with the
    /// next write. The trace is the one given back with `trace_back`, or
    /// else the one its wrapper (`Caught`) just handed back.
    #[cold]
    #[inline(never)]
    fn trace_answered(&mut self, status: u16) {
        let handed = handed_back();
        if self.traces.0 != 0 {
            with_traces(self.traces.0, |t| {
                if let Some(mut trace) = t.current.take().or(handed) {
                    trace.responded(status);
                    t.answered.push(*trace);
                }
            });
        }
    }

    /// Everything answered has been written.
    #[inline]
    fn trace_written(&mut self) {
        if self.traces.0 != 0 {
            self.trace_written_slow();
        }
    }

    #[cold]
    #[inline(never)]
    fn trace_written_slow(&mut self) {
        // Finishing a trace does not reach `TRACES`.
        with_traces(self.traces.0, |t| {
            for trace in t.answered.drain(..) {
                trace.finish();
            }
        });
    }

    /// The connection's task.
    ///
    /// The connection lives in this future and [`run`](Self::run) borrows
    /// it there. An `async fn` that took it by value would keep it twice
    /// (the argument, and the variable it is moved into), and every idle
    /// connection would pay for the copy.
    ///
    /// A connection of an accept loop that turns out to be HTTP/2 (the
    /// preface, with prior knowledge) goes on in a task of its own with what
    /// HTTP/2 needs of it, and this one ends: this task is as large as the
    /// HTTP/1.1 state, which HTTP/2 has no use for.
    fn serve<H: Handler>(self, handler: Rc<H>) -> impl Future<Output = ()> {
        let mut conn = self;
        async move {
            if let Some(input) = conn.run(&*handler).await {
                conn.hand_over_to_h2(handler, input);
            }
        }
    }

    /// Goes on with an accept loop's HTTP/2 connection in a task of its own
    /// (see [`serve`](Self::serve)). Out of line: inline, the spawn made the
    /// connection's future too large to poll from the executor's loop
    /// without a call, which every HTTP/1.1 request paid for.
    #[cold]
    #[inline(never)]
    fn hand_over_to_h2<H: Handler>(self, handler: Rc<H>, input: Vec<u8>) {
        let Conn {
            stream,
            cfg,
            peer,
            _slot: Some(slot),
            ..
        } = self
        else {
            unreachable!("only a connection with a slot is handed over");
        };
        drop(giong::spawn(serve_h2(
            stream,
            cfg,
            peer,
            handler,
            slot,
            move || input,
        )));
    }

    /// Serves the connection's requests until it closes. Returns what has
    /// been received of an HTTP/2 connection, which the caller serves, if
    /// the connection is an accept loop's (it has a slot); any other
    /// connection is served to the end here.
    async fn run<H: Handler>(&mut self, handler: &H) -> Option<Vec<u8>> {
        // Set when the connection closes with part of a request body unread.
        let mut linger = false;
        // Why the connection stops reading requests. Nothing is awaited
        // inside the `match` on `next_request`: the result it matches on (a
        // request, 200 bytes) would be kept in the connection's future across
        // that `.await`, beside every other state, idle or not.
        let end = loop {
            let (req, streamed) = match self.next_request(handler).await {
                Ok(next) => next,
                Err(end) => break end,
            };
            // What the connection needs from the request, read in a block of
            // its own: a local that has been borrowed is kept in the
            // connection's future across every later `.await`, so `req` must
            // only be moved from here on.
            let (minor, head_only, connect, wants_keep_alive, req) = {
                let req = req;
                let minor = req.head.minor;
                let method = req.method();
                let head_only = method == "HEAD";
                let connect = method == "CONNECT";
                let wants_keep_alive = !req.head.close && (minor == 1 || req.head.keep_alive);
                (minor, head_only, connect, wants_keep_alive, req)
            };

            let traced = self.traces.0 != 0;
            // Only small values cross an `.await` from here on: a local kept
            // across more than one `.await` has its room in every state of
            // this future, waiting for the next request included.
            let (resp, body_read) = match streamed {
                None => {
                    // Poll the handler once before deciding what to do with
                    // responses that are waiting: if it answers at once (the
                    // common case), they all go out in one write; if it has
                    // to wait, they go out first.
                    //
                    // The handler's future lives on the heap for the length
                    // of one request. Kept inside the connection's own
                    // future, it would make every connection as large as the
                    // largest handler future (a database query, a call to
                    // another service) even while idle.
                    let made = {
                        let mut trace = if traced { self.trace_handler() } else { None };
                        let made = catching(|| {
                            HeapFuture::with(|| Caught {
                                fut: handler.call(req),
                                trace: trace.take(),
                            })
                        });
                        if traced && made.is_err() {
                            self.trace_back(trace);
                        }
                        made
                    };
                    let answer = match made {
                        Ok(mut fut) => {
                            let first =
                                poll_fn(|cx| Poll::Ready(Pin::new(&mut fut).poll(cx))).await;
                            match first {
                                Poll::Ready(answer) => answer,
                                Poll::Pending => {
                                    // Moved whole: matched and left in place,
                                    // it would be kept across the waits.
                                    drop(first);
                                    if self.flush().await.is_err() {
                                        return None;
                                    }
                                    fut.await
                                }
                            }
                        }
                        Err(()) => Err(()),
                    };
                    match answer {
                        Ok(resp) => (resp, true),
                        // The connection closes after the answer, as after
                        // a body that was not read: requests behind it are
                        // dropped, and the client still reads the `500`.
                        Err(()) => (panicked(), false),
                    }
                }
                Some(body) => {
                    // The body may take long to arrive: answers to earlier
                    // requests go out first. On the heap like the handler's
                    // future, so that the rarer streaming paths do not make
                    // every connection larger.
                    let answer = HeapFuture::with(|| async {
                        self.flush().await?;
                        Ok::<_, End>(self.answer_streaming(handler, req, body).await)
                    });
                    match answer.await {
                        Ok(answered) => answered,
                        Err(_) => return None,
                    }
                }
            };
            // What the connection needs of the response, in a block of its
            // own like the request's above: borrowed, `resp` would be kept
            // in the connection's future across every later `.await`.
            let (stream, keep_alive) = {
                let mut resp = resp;
                if traced {
                    self.trace_answered(resp.status());
                }
                // Decided after the handler ran: a shutdown may have begun
                // meanwhile.
                self.closing |= giong::is_shutting_down();
                let streams = resp.is_streaming() && resp.sends_body(head_only);
                // Without chunked encoding (HTTP/1.0), a streamed body ends
                // when the connection does.
                // After a 2xx to CONNECT, the client and the proxies on the
                // way take what follows on the connection for a tunnel (RFC
                // 9110, section 9.3.6), which the server has none of: what a
                // proxy in front passes on blindly must not be read as
                // requests it never saw.
                let tunnel = connect && (200..300).contains(&resp.status());
                let keep_alive = wants_keep_alive
                    && !self.closing
                    && body_read
                    && !(streams && minor == 0)
                    && !tunnel;
                self.reserve_out();
                let stream = if streams {
                    resp.write_head(&mut self.out, minor, keep_alive);
                    match resp.take_body() {
                        Body::Stream(body) => Some(body),
                        _ => None,
                    }
                } else if resp.sends_body(head_only)
                    && (resp.body_len() >= SEPARATE_BODY_AT || resp.body_is_file())
                {
                    // Sent from where it is, right after its head.
                    resp.write_head(&mut self.out, minor, keep_alive);
                    self.body = resp.take_body();
                    None
                } else {
                    // A streamed body that is not sent (`HEAD`, 204, 304) is
                    // dropped here, which tells its sender.
                    resp.write_to(&mut self.out, minor, keep_alive, head_only);
                    None
                };
                (stream, keep_alive)
            };
            if let Some(body) = stream {
                if HeapFuture::with(|| self.write_stream(&body, minor == 1))
                    .await
                    .is_err()
                {
                    return None;
                }
            }
            if !keep_alive {
                linger = !body_read;
                break End::Close;
            }
            // Nothing may be appended after a separate body: send it now.
            let send_now = self.out.len() >= FLUSH_AT || !matches!(self.body, Body::Empty);
            if send_now && self.flush().await.is_err() {
                return None;
            }
        };
        match end {
            End::Close => {
                if self.flush().await.is_ok() {
                    if linger {
                        self.lingering_close().await;
                    } else {
                        self.close_write().await;
                    }
                }
            }
            End::Http2 => {
                let input = self.buf[self.pos..].to_vec();
                self.release_read_buffer_all();
                self.charge.shrink_to(0);
                if self._slot.is_some() {
                    return Some(input);
                }
                HeapFuture::with(|| {
                    crate::h2::serve(&self.stream, &self.cfg, &self.peer, handler, input, None)
                })
                .await;
            }
            End::Status(_) | End::Busy => {
                self.write_refusal(end);
                if self.flush().await.is_ok() {
                    self.lingering_close().await;
                }
            }
        }
        None
    }

    /// Writes the answer to a request that is refused without its handler
    /// (an error status, or `503` for a full budget). Not inline in `run`:
    /// a `Response` there would be kept in every connection's future.
    fn write_refusal(&mut self, end: End) {
        // Nothing more of the request is read: what it holds goes back now
        // rather than after the answer.
        self.release_read_buffer_all();
        self.charge.shrink_to(0);
        self.reserve_out();
        let resp = match end {
            End::Status(status) => Response::new(status),
            _ => busy(),
        };
        resp.write_to(&mut self.out, 1, false, false);
    }

    /// Runs the handler of a request whose body it reads as a stream, while
    /// the connection reads the body into the stream. Returns the response,
    /// and whether the body was read to its end: only then can another
    /// request follow on the connection.
    async fn answer_streaming<H: Handler>(
        &mut self,
        handler: &H,
        req: Request,
        body: Streamed,
    ) -> (Response, bool) {
        let mut pumped = None;
        // Out of the connection, which the pump borrows, while the handler
        // runs.
        let trace = self.trace_handler();
        let traced = trace.is_some();
        let answer = match catching(|| handler.call(req)) {
            Ok(fut) => {
                let mut pump = pin!(self.pump(&body));
                let mut fut = pin!(Caught { fut, trace });
                poll_fn(|cx| {
                    if pumped.is_none() {
                        if let Poll::Ready(result) = pump.as_mut().poll(cx) {
                            pumped = Some(result);
                        }
                    }
                    fut.as_mut().poll(cx)
                })
                .await
            }
            Err(()) => {
                self.trace_back(trace);
                Err(())
            }
        };
        if traced {
            self.trace_back(handed_back());
        }
        // If the handler answered first, nothing reads the rest of the body;
        // a stream it kept says so. (The first end counts.)
        body.pipe.finish(Err(Failure::Abandoned));
        match answer {
            Ok(resp) => (resp, pumped == Some(Ok(()))),
            // The rest of the body is not read, so the connection closes.
            Err(()) => (panicked(), false),
        }
    }

    /// Reads a streamed request body into its pipe, piece by piece, from
    /// the moment the handler first asks for it.
    async fn pump(&mut self, body: &Streamed) -> Result<(), Failure> {
        body.pipe.asked().await;
        let result = if body.chunked {
            match self.pump_chunked(body).await {
                Ok(trailers) => {
                    body.pipe.finish_with(Ok(()), Some(trailers));
                    return Ok(());
                }
                Err(failure) => Err(failure),
            }
        } else {
            self.pump_length(body).await
        };
        body.pipe.finish(result);
        result
    }

    async fn pump_length(&mut self, body: &Streamed) -> Result<(), Failure> {
        let mut left = body.length;
        // What arrived with the head, which counts for `min_body_rate` as
        // what arrives later does.
        let have = (self.buf.len() - self.pos).min(usize::try_from(left).unwrap_or(usize::MAX));
        let mut clock = RateClock {
            got: have as u64,
            ..RateClock::default()
        };
        if have > 0 {
            let piece = self.buf[self.pos..self.pos + have].to_vec();
            self.pos += have;
            left -= have as u64;
            body.pipe.put(piece).await;
        }
        if left > 0 {
            self.continue_body(body).await?;
        }
        while left > 0 {
            let chunk = self.recv_body(&mut clock).await?;
            let n = usize::try_from(left).map_or(chunk.len(), |left| left.min(chunk.len()));
            let piece = chunk[..n].to_vec();
            if n < chunk.len() {
                // The start of the next request.
                self.append(&chunk[n..]);
            }
            drop(chunk);
            left -= n as u64;
            body.pipe.put(piece).await;
        }
        Ok(())
    }

    async fn pump_chunked(&mut self, body: &Streamed) -> Result<Trailers, Failure> {
        let mut decoder = Decoder::new(usize::try_from(body.limit).unwrap_or(usize::MAX));
        // What arrived with the head, then what follows, less what the
        // decoder has read.
        let mut input = self.buf[self.pos..].to_vec();
        self.pos = self.buf.len();
        let mut waited = false;
        // What arrived with the head counts for `min_body_rate` as what
        // arrives later does: every byte of the chunked body, framing too.
        let mut clock = RateClock {
            got: input.len() as u64,
            ..RateClock::default()
        };
        loop {
            let progress = decoder.feed(&input);
            let piece = mem::take(&mut decoder.body);
            if !piece.is_empty() {
                body.pipe.put(piece).await;
            }
            match progress {
                Ok(Progress::Done(used)) => {
                    if used < input.len() {
                        // The start of the next request.
                        self.append(&input[used..]);
                    }
                    return Ok(decoder.trailers.take().map(|t| *t).unwrap_or_default());
                }
                Ok(Progress::NeedMore) => {
                    input.drain(..decoder.consume());
                    if !waited {
                        waited = true;
                        self.continue_body(body).await?;
                    }
                    let chunk = self.recv_body(&mut clock).await?;
                    input.extend_from_slice(&chunk);
                }
                Err(413) => return Err(Failure::TooLarge(body.limit)),
                Err(_) => return Err(Failure::Malformed),
            }
        }
    }

    /// Answers `Expect: 100-continue` before waiting for a streamed body.
    async fn continue_body(&mut self, body: &Streamed) -> Result<(), Failure> {
        if body.expect_continue {
            self.reserve_out();
            self.out.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
            self.flush().await.map_err(|_| Failure::Cut)?;
        }
        Ok(())
    }

    /// The next piece of a streamed request body, within the request
    /// timeout, and within `min_body_rate` when it is on (`clock` counts
    /// what the body has received and waited for).
    async fn recv_body(&self, clock: &mut RateClock) -> Result<T::Chunk, Failure> {
        let rate = self.cfg.min_body_rate;
        if !rate.is_on() {
            return match timeout(self.cfg.request_timeout, self.stream.recv()).await {
                Ok(Ok(chunk)) if !chunk.is_empty() => Ok(chunk),
                _ => Err(Failure::Cut),
            };
        }
        let (wait, by_rate) = rate.wait(clock, self.cfg.request_timeout);
        let start = Instant::now();
        let received = timeout(wait, self.stream.recv()).await;
        let now = Instant::now();
        clock.waited += now - start;
        match received {
            Ok(Ok(chunk)) if !chunk.is_empty() => {
                clock.got += chunk.len() as u64;
                Ok(chunk)
            }
            Err(_) if by_rate => Err(Failure::TooSlow(rate.per_sec)),
            _ => Err(Failure::Cut),
        }
    }

    /// Writes a streamed response body as its producer sends it, after the
    /// head that is in `out`: each piece as a chunk (as it is, for HTTP/1.0),
    /// written as soon as it arrives. `Err` if the client stopped reading
    /// or the producer aborted: the connection then closes without ending
    /// the body, so the client can tell it is incomplete.
    async fn write_stream(&mut self, body: &StreamBody, chunked: bool) -> Result<(), End> {
        // A large piece went out without the CRLF that ends its chunk.
        let mut open = false;
        loop {
            let mut next = pin!(body.next());
            let next = match poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await {
                Poll::Ready(next) => next,
                Poll::Pending => {
                    // Nothing more yet: what is waiting goes out now.
                    self.flush().await?;
                    next.await
                }
            };
            // Nothing may be appended after a separate body.
            if !matches!(self.body, Body::Empty) {
                self.flush().await?;
            }
            self.reserve_out();
            if open {
                self.out.extend_from_slice(b"\r\n");
                open = false;
            }
            match next {
                Next::Piece(piece) => {
                    if chunked {
                        push_hex(&mut self.out, piece.len());
                        self.out.extend_from_slice(b"\r\n");
                    }
                    if piece.len() >= SEPARATE_BODY_AT {
                        // Sent from where it is.
                        self.body = Body::Owned(piece);
                        open = chunked;
                    } else {
                        self.out.extend_from_slice(&piece);
                        if chunked {
                            self.out.extend_from_slice(b"\r\n");
                        }
                    }
                }
                Next::End(trailers) => {
                    if chunked {
                        self.out.extend_from_slice(b"0\r\n");
                        if let Some(trailers) = trailers {
                            trailers.write_lines(&mut self.out);
                        }
                        self.out.extend_from_slice(b"\r\n");
                    }
                    return Ok(());
                }
                Next::Aborted => {
                    let _ = self.flush().await;
                    return Err(End::Close);
                }
            }
        }
    }

    /// Receives and parses the next request, body included, unless the
    /// handler reads the body as a stream: the second value then says how.
    async fn next_request<H: Handler>(
        &mut self,
        handler: &H,
    ) -> Result<(Request, Option<Streamed>), End> {
        let mut blank_lines = 0;
        let mut head = loop {
            // Empty lines before a request line are ignored (RFC 9112 §2.2).
            while self.buf[self.pos..].starts_with(b"\r\n") {
                self.pos += 2;
                self.scanned = 0;
                blank_lines += 1;
            }
            let data = &self.buf[self.pos..];
            if self.first && self.cfg.http2 && self.cfg.http2_prior_knowledge && !data.is_empty() {
                // HTTP/2 with prior knowledge: the preface, before anything
                // else was sent on the connection.
                let n = data.len().min(PREFACE.len());
                if data[..n] == PREFACE[..n] {
                    if n == PREFACE.len() {
                        return Err(End::Http2);
                    }
                    self.read_more(false).await?;
                    continue;
                }
                self.first = false;
            }
            let data = if !data.is_empty() && RequestTrace::is_on() {
                self.trace_arrival();
                &self.buf[self.pos..]
            } else {
                data
            };
            if let Some(end) = find_head_end(data, self.scanned) {
                if end > self.cfg.max_head_size {
                    return Err(End::Status(431));
                }
                break parse_head(&data[..end]).map_err(End::Status)?;
            }
            if data.len() > self.cfg.max_head_size {
                return Err(End::Status(431));
            }
            // The terminator may straddle the next read.
            self.scanned = data.len().saturating_sub(3);
            // A client may end a body with a stray CRLF and then go quiet, so
            // a blank line or two still leave the connection idle. More start
            // the request timeout: otherwise a CRLF every minute would hold
            // the connection open for ever.
            let idle = data.is_empty() && blank_lines <= IDLE_BLANK_LINES;
            self.read_more(idle).await?;
        };
        if self.head_deadline {
            // The first request's head is in within `first_request_timeout`;
            // its body gets `request_timeout`, as any other.
            self.head_deadline = false;
            self.deadline = None;
        }

        if head.chunked || head.content_length.is_some_and(|len| len > 0) {
            let start = self.pos + head.len;
            let mut req = Request {
                buf: self.buf.clone(),
                start: self.pos,
                head,
                body: BodyBytes::InBuffer(start..start),
                peer: self.peer,
                charge: Charge::none(),
                early_data: false,
            };
            let stream_body =
                catching(|| handler.stream_body(&req)).map_err(|()| End::Status(500))?;
            match stream_body {
                Some(limit) => {
                    let length = req.head.content_length;
                    if length.is_some_and(|len| len > limit) {
                        return Err(End::Status(413));
                    }
                    let pipe = Rc::new(Pipe::new(length));
                    let streamed = Streamed {
                        pipe: pipe.clone(),
                        chunked: req.head.chunked,
                        length: length.unwrap_or(0),
                        limit,
                        expect_continue: req.head.expect_continue && req.head.minor == 1,
                    };
                    req.body = BodyBytes::Stream(pipe);
                    // The body is the pump's to read from here.
                    self.pos += req.head.len;
                    self.scanned = 0;
                    self.deadline = None;
                    self.settle();
                    return Ok((req, Some(streamed)));
                }
                None => head = req.head,
            }
        }

        // The body, if there is none or all of it is in the read buffer;
        // otherwise what is left to receive.
        let at_hand = if head.chunked {
            let mut decoder = Decoder::new(self.cfg.max_body_size);
            let data = &self.buf[self.pos + head.len..];
            // The decoded body is held as well as the bytes it came from.
            let progress = decoder.feed(data).map_err(End::Status)?;
            self.account(decoder.body.len())?;
            match progress {
                Progress::Done(used) => Ok((BodyBytes::Decoded(decoder.body), head.len + used)),
                Progress::NeedMore => Err(Wait::Chunked(decoder)),
            }
        } else {
            let len = head.content_length.unwrap_or(0);
            if len > self.cfg.max_body_size as u64 {
                return Err(End::Status(413));
            }
            let len = len as usize;
            // Refused before the body is read (and before `100 Continue`)
            // if it cannot fit in the worker's budget.
            self.admit(head.len + len)?;
            if self.buf.len() - self.pos - head.len < len {
                Err(Wait::Length(len))
            } else {
                let start = self.pos + head.len;
                Ok((BodyBytes::InBuffer(start..start + len), head.len + len))
            }
        };
        // A body still on its way is received in a future on the heap: its
        // states (the decoder of a chunked body above all) would otherwise
        // be kept in every connection's future, idle or not.
        let (body, total) = match at_hand {
            Ok(at_hand) => at_hand,
            Err(wait) => HeapFuture::with(|| self.read_body(&head, wait)).await?,
        };

        // What the request holds goes with it, and is given back when the
        // handler drops it; the connection keeps the share of whatever
        // follows it in the buffer.
        let charge = if self.charge.bytes() == 0 {
            // A request within `FREE`: the usual case, and nothing to work out.
            Charge::none()
        } else {
            let rest = self.buf.len() - self.pos - total;
            self.charge.split_off(rest.saturating_sub(FREE) as u64)
        };
        let req = Request {
            buf: self.buf.clone(),
            start: self.pos,
            head,
            body,
            peer: self.peer,
            charge,
            early_data: false,
        };
        self.pos += total;
        self.scanned = 0;
        self.deadline = None;
        Ok((req, None))
    }

    /// Receives the rest of the body of the request whose head is `head`,
    /// from where `next_request` left it (`wait`).
    async fn read_body(&mut self, head: &Head, wait: Wait) -> Result<(BodyBytes, usize), End> {
        let mut interim_sent = false;
        let len = match wait {
            Wait::Chunked(mut decoder) => loop {
                self.send_continue(head, &mut interim_sent);
                self.read_more(false).await?;
                let data = &self.buf[self.pos + head.len..];
                let progress = decoder.feed(data).map_err(End::Status)?;
                self.account(decoder.body.len())?;
                if let Progress::Done(used) = progress {
                    return Ok((BodyBytes::Decoded(decoder.body), head.len + used));
                }
            },
            Wait::Length(len) => len,
        };
        if len >= RECV_INTO_AT {
            self.body_into = true;
            self.scanned = head.len + len;
        }
        while self.buf.len() - self.pos - head.len < len {
            self.send_continue(head, &mut interim_sent);
            self.read_more(false).await?;
        }
        self.body_into = false;
        let start = self.pos + head.len;
        Ok((BodyBytes::InBuffer(start..start + len), head.len + len))
    }

    /// Receives until the read buffer holds `want` bytes from `pos` (a
    /// request with a large body), straight into the buffer: the body is not
    /// copied out of the runtime's receive buffers. Leaves the buffer as it
    /// is if a handler still holds a request in it; the caller's usual loop
    /// then receives the rest.
    async fn recv_body_into(&mut self, want: usize) -> Result<(), End> {
        let Some(buf) = Rc::get_mut(&mut self.buf) else {
            return Ok(());
        };
        if self.pos > 0 {
            buf.drain(..self.pos);
            self.pos = 0;
        }
        let request_timeout = self.cfg.request_timeout;
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + request_timeout);
        while self.buf.len() < want {
            let Some(buf) = Rc::get_mut(&mut self.buf) else {
                return Ok(());
            };
            let mut buf = mem::take(buf);
            // Room for more of the body: as much as has arrived (which the
            // budget charges), at least 1 MiB and at most 8 MiB. A client
            // that announces a large body and sends little is not given room
            // for what it announced. Each step past the first may copy the
            // buffer, so they are not smaller: growing from 64 KiB costs a
            // third of the 1 MiB uploads per second on io_uring.
            if buf.capacity() == buf.len() {
                let step = buf.len().clamp(FIRST_STEP, RESERVE_STEP);
                buf.reserve_exact((want - buf.len()).min(step));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let (res, back) = match timeout(left, self.stream.recv_into(buf)).await {
                Ok(received) => received,
                Err(_) => return Err(End::Status(408)),
            };
            *Rc::get_mut(&mut self.buf).expect("the read buffer is not shared") = back;
            match res {
                Ok(n) if n > 0 => {}
                // Closed by the peer, or an I/O error.
                _ => return Err(End::Close),
            }
            // Charged as it arrives, like what `read_more` receives: past the
            // worker's budget for requests being received, the connection is
            // refused (`Busy`), so stalled uploads cannot hold more than it.
            self.account(0)?;
        }
        Ok(())
    }

    /// Answers `Expect: 100-continue` once, before waiting for the body.
    fn send_continue(&mut self, head: &crate::parse::Head, sent: &mut bool) {
        if head.expect_continue && head.minor == 1 && !*sent {
            *sent = true;
            self.reserve_out();
            self.out.extend_from_slice(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
    }

    /// Sends pending responses, then waits for more input. `idle` means that
    /// no part of a request has been received yet.
    async fn read_more(&mut self, idle: bool) -> Result<(), End> {
        if idle {
            // Every request received has been parsed: the read buffer goes
            // back to the pool now rather than after the send, so that the
            // connections of one turn of the event loop share fewer buffers.
            self.release_read_buffer();
        }
        self.flush().await?;
        if self.body_into && Rc::get_mut(&mut self.buf).is_some() {
            // Boxed, so that it adds nothing to the state every waiting
            // connection keeps.
            return Box::pin(self.recv_body_into(self.scanned)).await;
        }
        let received = if idle {
            if self.closing {
                return Err(End::Close);
            }
            give_buffer(mem::take(&mut self.out));
            // At `max_connections`, the accept loop may shut the reading side
            // of an idle connection down: it then reads the end of its
            // stream here, and closes.
            if let Some(slot) = &self._slot {
                slot.set_idle(true);
            }
            // Nothing of a request has arrived, so a deadline is
            // `first_request_timeout`'s: the connection has sent nothing yet,
            // and is closed without an answer when it passes. Otherwise the
            // idle timeout, from the worker's clock of this turn: never more
            // than the turn has run so far before now.
            let deadline = match self.deadline {
                Some(deadline) => deadline,
                None => giong::time::turn_clock() + self.cfg.idle_timeout,
            };
            // Shutting down: a request that was already on its way is still
            // answered (with `Connection: close`, as every response once the
            // shutdown has begun), then the connection closes.
            let waited = self
                .recv_until(deadline, Some(self.cfg.shutdown_linger))
                .await;
            if let Some(slot) = &self._slot {
                slot.set_idle(false);
            }
            match waited {
                Waited::Received(received) => received,
                Waited::TimedOut => return Err(End::Close),
            }
        } else {
            let request_timeout = self.cfg.request_timeout;
            let deadline = *self
                .deadline
                .get_or_insert_with(|| Instant::now() + request_timeout);
            // Unlike the wait for a request, the rest of one is not cut
            // short by a shutdown: it is answered.
            match self.recv_until(deadline, None).await {
                Waited::Received(received) => received,
                Waited::TimedOut => return Err(End::Status(408)),
            }
        };
        match received {
            Ok(chunk) if !chunk.is_empty() => {
                self.append(&chunk);
                drop(chunk);
                self.account(0)
            }
            // Idle for too long, an I/O error, or closed by the peer.
            _ => Err(End::Close),
        }
    }

    /// Receives the next chunk unless `deadline` passes first, with the
    /// connection's alarm: a wait arms no timer and registers no shutdown
    /// waker of its own, and a later deadline than the last wait's costs
    /// nothing in the timer wheel. At a shutdown, the deadline becomes
    /// `linger` from then, or stays (`None`). The alarm stays armed after
    /// the wait (it is dropped with the connection), so between requests
    /// the task may be woken once by an old deadline: every await of the
    /// connection is fine with a spurious wake-up.
    fn recv_until(
        &mut self,
        mut deadline: Instant,
        mut linger: Option<Duration>,
    ) -> impl Future<Output = Waited<T::Chunk>> + '_ {
        let recv = self.stream.recv();
        let alarm = &mut self.alarm;
        let mut shut = false;
        async move {
            let mut recv = pin!(recv);
            poll_fn(move |cx| {
                // What was received first, before the deadline, as a timeout
                // does, and before a shutdown. The alarm is armed only when
                // there is a wait, and keeps an earlier deadline while input
                // is ready (it then wakes the connection once in vain).
                if let Poll::Ready(received) = recv.as_mut().poll(cx) {
                    return Poll::Ready(Waited::Received(received));
                }
                loop {
                    let rang = if shut {
                        alarm.poll_deadline(deadline, cx)
                    } else {
                        alarm.poll_until(deadline, cx)
                    };
                    match rang {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Rang::Deadline) => return Poll::Ready(Waited::TimedOut),
                        Poll::Ready(Rang::Shutdown) => {
                            shut = true;
                            if let Some(linger) = linger.take() {
                                deadline = Instant::now() + linger;
                            }
                        }
                    }
                }
            })
            .await
        }
    }

    /// Adds received bytes to the buffer, dropping what has been consumed.
    fn append(&mut self, chunk: &[u8]) {
        match Rc::get_mut(&mut self.buf) {
            Some(buf) => {
                if self.pos > 0 {
                    buf.drain(..self.pos);
                    self.pos = 0;
                }
                if buf.capacity() == 0 {
                    *buf = take_buffer();
                }
                buf.extend_from_slice(chunk);
            }
            // A handler kept a request alive: leave its buffer alone.
            None => {
                let mut fresh = take_buffer();
                fresh.extend_from_slice(&self.buf[self.pos..]);
                fresh.extend_from_slice(chunk);
                self.buf = Rc::new(fresh);
                self.pos = 0;
            }
        }
    }

    /// Charges the unparsed data, less the first `FREE` bytes, and `extra`
    /// (a body being decoded) against the worker's budget. `Busy` if that
    /// would take the worker past its limit.
    fn account(&mut self, extra: usize) -> Result<(), End> {
        let unparsed = self.buf.len() - self.pos;
        if unparsed <= FREE && extra == 0 {
            return Ok(());
        }
        let held = (unparsed.saturating_sub(FREE) + extra) as u64;
        if held <= self.charge.bytes() {
            return Ok(());
        }
        let limit = self.cfg.buffered_limit();
        if self.charge.grow_to(held, limit) {
            Ok(())
        } else {
            budget::note_refused(limit);
            Err(End::Busy)
        }
    }

    /// Whether a request taking `total` bytes from `pos` can be received:
    /// `413` if it is larger than the worker's whole budget, `Busy` if the
    /// budget cannot take it now. Nothing is charged: bytes are, as they
    /// arrive.
    fn admit(&mut self, total: usize) -> Result<(), End> {
        let held = (total.saturating_sub(FREE)) as u64;
        let Some(more) = held
            .checked_sub(self.charge.bytes())
            .filter(|&more| more > 0)
        else {
            return Ok(());
        };
        let limit = self.cfg.buffered_limit();
        if held > limit {
            return Err(End::Status(413));
        }
        if budget::fits(more, limit) {
            Ok(())
        } else {
            budget::note_refused(limit);
            Err(End::Busy)
        }
    }

    /// Gives back what is charged beyond the unparsed data's share.
    fn settle(&mut self) {
        let held = (self.buf.len() - self.pos).saturating_sub(FREE);
        self.charge.shrink_to(held as u64);
    }

    fn reserve_out(&mut self) {
        if self.out.capacity() == 0 {
            self.out = take_buffer();
        }
    }

    /// Hands the read buffer back, whatever is left in it.
    fn release_read_buffer_all(&mut self) {
        self.pos = self.buf.len();
        self.release_read_buffer();
    }

    /// Hands the read buffer back to the worker's pool if all of it has
    /// been parsed. A handler that still holds a request keeps its copy.
    fn release_read_buffer(&mut self) {
        if self.pos == self.buf.len() {
            match Rc::get_mut(&mut self.buf) {
                Some(buf) => give_buffer(mem::take(buf)),
                None => self.buf = Rc::new(Vec::new()),
            }
            self.pos = 0;
            self.charge.shrink_to(0);
        }
    }

    /// Writes everything in `out`. One send per batch of responses.
    async fn flush(&mut self) -> Result<(), End> {
        if self.out.is_empty() && matches!(self.body, Body::Empty) {
            self.trace_written();
            return Ok(());
        }
        let out = mem::take(&mut self.out);
        // The write timeout counts from the write's last progress, as on
        // HTTP/2 and HTTP/3: a large body to a slow client may take longer
        // than the timeout in all, as long as the client keeps taking it.
        let idle = &self.cfg.write_timeout;
        // A body sent from where it is (a file, or 32 KiB and more) is
        // written from the heap: its write futures are the largest, and
        // inline they would make every connection's future larger, idle or
        // not, for the rarer answers.
        //
        // No `match` on the body taken out of `self.body` around the plain
        // write: the value matched would be kept across its `.await` too.
        let wrote = if matches!(self.body, Body::Empty) {
            until_stalled(idle, self.stream.write_all(out)).await
        } else {
            match mem::take(&mut self.body) {
                Body::File(f) => {
                    let stream = &self.stream;
                    HeapFuture::with(|| {
                        until_stalled(
                            idle,
                            stream.write_all_with_file(out, &f.file, f.offset, f.len),
                        )
                    })
                    .await
                }
                body => {
                    let stream = &self.stream;
                    HeapFuture::with(|| until_stalled(idle, stream.write_all_with_body(out, body)))
                        .await
                }
            }
        };
        match wrote {
            Some((Ok(()), mut out)) => {
                out.clear();
                self.out = out;
                self.trace_written();
                Ok(())
            }
            _ => Err(End::Close),
        }
    }

    /// Tells the peer that nothing more will be sent: a FIN, and over TLS a
    /// `close_notify` before it, so that the client can tell a complete
    /// response from a connection that was cut off.
    async fn close_write(&mut self) {
        let _ = timeout(self.cfg.write_timeout, self.stream.close_write()).await;
    }

    /// Closing a socket that still has unread input makes the kernel send a
    /// reset, which can destroy the error response before the client reads it.
    /// So: stop sending, then discard input for a moment.
    async fn lingering_close(&mut self) {
        self.close_write().await;
        let deadline = Instant::now() + ERROR_LINGER;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match timeout(left, self.stream.recv()).await {
                Ok(Ok(chunk)) if !chunk.is_empty() => {}
                _ => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shared handler answers through the handler it shares, and keeps its
    /// choice of streaming: a closure around it would lose that.
    #[test]
    fn a_shared_handler_keeps_its_streaming_routes() {
        struct Streams;
        impl Handler for Streams {
            async fn call(&self, _req: Request) -> Response {
                Response::new(204)
            }
            fn stream_body(&self, _req: &Request) -> Option<u64> {
                Some(5)
            }
        }
        let shared = Rc::new(Streams);
        let other = Rc::clone(&shared);
        let req = Request::from_bytes(b"POST / HTTP/1.1\r\nHost: a\r\n\r\n").unwrap();
        assert_eq!(other.stream_body(&req), Some(5));
        let status = giong::block_on(async { shared.call(req).await.status() });
        assert_eq!(status, 204);
    }

    /// `HANDED_BACK` is empty when a traced handler starts: a trace left
    /// there by a path that did not take it back is dropped (counted as
    /// abandoned) rather than adopted by the next request, and this crate's
    /// tests fail on it (issue #374).
    #[test]
    fn a_stale_handed_back_trace_is_not_adopted() {
        let leftover = giong::block_on(async {
            giong::handle().set_request_tracing(true);
            giong::yield_now().await;
            let trace = RequestTrace::start("test").expect("tracing is on");
            HANDED_BACK.set(Some(Box::new(trace)));
            let cleared = catch_unwind(clear_handed_back);
            giong::handle().set_request_tracing(false);
            (cleared.is_err(), handed_back().is_none())
        });
        assert_eq!(leftover, (true, true));
    }

    /// A connection's place in the accept loop's bookkeeping follows the
    /// connections open at once, not its file descriptor number, and a
    /// freed place is taken again first.
    #[test]
    fn connection_places_are_reused_whatever_the_descriptor() {
        giong::block_on(async {
            let slots = Rc::new(Slots::new(1000, 0));
            let mut held = Vec::new();
            for i in 0..130 {
                let mut slot = slots.take(false).await;
                slot.set_fd(1_000_000 + i);
                assert_eq!(slot.place, i as u32);
                held.push(slot);
            }
            assert_eq!(slots.places.borrow().len(), 130);
            assert_eq!(slots.used.borrow().len(), 3);
            // Free one place in the first word and one in the second.
            drop(held.remove(70));
            drop(held.remove(5));
            let mut slot = slots.take(false).await;
            slot.set_fd(7);
            assert_eq!(slot.place, 5);
            held.push(slot);
            let mut slot = slots.take(false).await;
            slot.set_fd(8);
            assert_eq!(slot.place, 70);
            held.push(slot);
            let mut slot = slots.take(false).await;
            slot.set_fd(9);
            assert_eq!(slot.place, 130);
            held.push(slot);
            assert_eq!(slots.places.borrow().len(), 131);
            // A connection that has received nothing is not marked idle.
            held[0].set_idle(true);
            assert_eq!(slots.idle.borrow()[0], 0);
            // Idle marks and the age order travel with the place.
            held[0].set_idle(false);
            held[1].allow_idle();
            held[0].set_idle(true);
            held[1].set_idle(true);
            held[0].set_idle(false);
            assert_eq!(slots.idle.borrow()[0], 0b10);
            drop(held);
            assert_eq!(slots.open.get(), 0);
            assert!(slots.used.borrow().iter().all(|&w| w == 0));
            assert!(slots.idle.borrow().iter().all(|&w| w == 0));
        });
    }

    /// `max_connections_per_ip` counts the connections open per client,
    /// gives a place back when its connection closes, and keeps no entry
    /// for a client with none open.
    #[test]
    fn connections_are_counted_per_client_while_open() {
        giong::block_on(async {
            let slots = Rc::new(Slots::new(1000, 2));
            let a: IpAddr = "203.0.113.7".parse().unwrap();
            let b: IpAddr = "2001:db8::1".parse().unwrap();
            let b_too: IpAddr = "2001:db8::2".parse().unwrap();
            let open = |fd: i32, ip: IpAddr| {
                let slots = slots.clone();
                async move {
                    let mut slot = slots.take(false).await;
                    slot.set_fd(fd);
                    let counted = slot.count_ip(Some(ip));
                    (slot, counted)
                }
            };
            let (a1, ok) = open(10, a).await;
            assert!(ok);
            let (a2, ok) = open(11, a).await;
            assert!(ok);
            assert!(slots.address_full(Some(a)));
            let (a3, ok) = open(12, a).await;
            assert!(!ok, "a third connection from one address");
            drop(a3);
            // Another client, as one IPv6 /64.
            let (b1, ok) = open(13, b).await;
            assert!(ok);
            let (b2, ok) = open(14, b_too).await;
            assert!(ok);
            assert!(slots.address_full(Some(b)));
            assert!(!slots.address_full(None));
            // A place given back counts again.
            drop(a1);
            assert!(!slots.address_full(Some(a)));
            let (a4, ok) = open(10, a).await;
            assert!(ok);
            drop((a2, a4, b1, b2));
            let per_ip = slots.per_ip.as_ref().unwrap();
            assert!(per_ip.open.borrow().is_empty());
            assert!(per_ip.keys.borrow().iter().all(Option::is_none));
            assert_eq!(slots.open.get(), 0);
        });
    }

    /// The accept loop looks for input waiting on a socket without taking
    /// it and without blocking.
    #[cfg(unix)]
    #[test]
    fn input_waiting_on_a_socket_is_seen_and_left_there() {
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        let (mut a, mut b) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(!has_input(b.as_raw_fd()));
        a.write_all(b"GET").unwrap();
        assert!(has_input(b.as_raw_fd()));
        assert!(has_input(b.as_raw_fd()));
        let mut got = [0u8; 3];
        b.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"GET");
        assert!(!has_input(b.as_raw_fd()));
    }

    /// The fixed refusal for low memory announces its own length.
    #[test]
    fn the_low_memory_answer_announces_its_length() {
        let text = std::str::from_utf8(LOW_MEMORY).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .unwrap();
        assert_eq!(length.parse::<usize>().unwrap(), body.len());
        assert!(body.starts_with("GIONG-E1121"));
    }

    /// The fixed refusal under `max_connections_per_ip` announces its own
    /// length.
    #[test]
    fn the_address_limit_answer_announces_its_length() {
        let text = std::str::from_utf8(TOO_MANY_FROM_ADDRESS).unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap();
        let length = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .unwrap();
        assert_eq!(length.parse::<usize>().unwrap(), body.len());
        assert!(head.starts_with("HTTP/1.1 429 "));
        assert!(body.starts_with("GIONG-E1124"));
    }

    /// An IPv4 address is one client, an IPv6 /64 network another; an
    /// IPv4-mapped IPv6 address is its IPv4 address; no two kinds meet.
    #[test]
    fn clients_are_counted_by_ipv4_address_and_ipv6_network() {
        let key = |s: &str| ip_key(s.parse().unwrap());
        assert_eq!(key("203.0.113.7"), key("::ffff:203.0.113.7"));
        assert_ne!(key("203.0.113.7"), key("203.0.113.8"));
        assert_eq!(
            key("2001:db8:1:2::1"),
            key("2001:db8:1:2:ffff:ffff:ffff:ffff")
        );
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
        assert_ne!(key("127.0.0.1"), key("::1"));
        assert_ne!(key("0.0.0.0"), key("::"));
        // The /64 an IPv4 key would stand for is multicast.
        assert_eq!(key("203.0.113.7") >> 56, 0xff);
    }

    /// What an idle connection costs is mostly its task, the connection's
    /// future. That future must not grow with the handler's (which lives on
    /// the heap only while a request is handled) or keep copies of itself.
    #[test]
    fn a_connection_future_stays_small_whatever_the_handler() {
        giong::Builder::new()
            .block_on(async {
                let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
                let addr = listener.local_addr().expect("address");
                let client = giong::spawn(async move { TcpStream::connect(addr).await });
                let stream = listener.accept().await.expect("accept");
                let _client = client.await.expect("task").expect("connect");

                // A handler that keeps 16 KiB across an `.await`.
                let handler = |_req: Request| async move {
                    let state = [7u8; 16 * 1024];
                    giong::time::sleep(Duration::from_millis(1)).await;
                    Response::new(200 + u16::from(std::hint::black_box(&state)[0] - 7))
                };
                let conn =
                    Conn::new(stream, Rc::new(Server::new().cfg), None).serve(Rc::new(handler));
                let size = mem::size_of_val(&conn);
                assert!(size <= 2048, "a connection's future takes {size} bytes");
            })
            .expect("runtime");
    }
}
