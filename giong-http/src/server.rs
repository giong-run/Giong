//! The accept loop and the per-connection state machine.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::future::{poll_fn, Future};
use std::io;
use std::mem;
use std::net::{Shutdown, SocketAddr};
use std::ops::Deref;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use giong::time::{sleep, timeout};

use crate::chunked::{Decoder, Progress};
use crate::heap::HeapFuture;
use crate::parse::{find_head_end, parse_head};
use crate::request::{BodyBytes, Request};
use crate::response::{Body, Response};

/// Responses to pipelined requests are sent together, up to this many bytes.
const FLUSH_AT: usize = 64 * 1024;
/// A body at least this large is not copied after its head: the two go out
/// in one vectored write instead (see `Transport::write_all_with_body`).
const SEPARATE_BODY_AT: usize = 32 * 1024;
/// After an error response, how long the connection keeps discarding input so
/// that the client can read the response instead of getting a reset.
const ERROR_LINGER: Duration = Duration::from_secs(1);
/// Blank lines before a request that still count as an idle connection.
const IDLE_BLANK_LINES: usize = 2;

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

fn take_buffer() -> Vec<u8> {
    BUFFERS
        .with(|pool| {
            let mut pool = pool.borrow_mut();
            let buf = pool.buffers.pop()?;
            pool.bytes -= buf.capacity();
            Some(buf)
        })
        .unwrap_or_else(|| Vec::with_capacity(BUFFER_SIZE))
}

fn give_buffer(mut buf: Vec<u8>) {
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
    /// both with one vectored write instead, without the copy.
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

    /// Closes the sending direction, so the peer sees the end of the stream
    /// while the server can still read what it sends.
    fn close_write(&self) -> impl Future<Output = ()>;

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

    async fn write_all_with_body(&self, head: Vec<u8>, body: Body) -> (io::Result<()>, Vec<u8>) {
        let bufs = vec![Cow::Owned(head), body.into_cow()];
        let (res, bufs) = TcpStream::write_all_vectored(self, bufs).await;
        (res, into_head(bufs))
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
    type Chunk = Vec<u8>;

    fn recv(&self) -> impl Future<Output = io::Result<Self::Chunk>> {
        giong_tls::TlsStream::recv(self)
    }

    fn write_all(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<()>, Vec<u8>)> {
        giong_tls::TlsStream::write_all(self, buf)
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

#[derive(Clone, Copy, Debug)]
struct Config {
    idle_timeout: Duration,
    request_timeout: Duration,
    write_timeout: Duration,
    max_head_size: usize,
    max_body_size: usize,
    shutdown_linger: Duration,
    max_connections: usize,
}

/// An HTTP/1.1 server. Configure it, then call [`serve`](Server::serve) from
/// each worker of a giong runtime.
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
    cfg: Config,
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
                max_connections: usize::MAX,
            },
        }
    }

    /// How long a keep-alive connection may sit between requests before it is
    /// closed (default: 60 s).
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.idle_timeout = timeout;
        self
    }

    /// How long a client may take to send one complete request, counted from
    /// the moment the server first has to wait for more of it (default: 30 s).
    /// Slower clients get `408 Request Timeout`.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.request_timeout = timeout;
        self
    }

    /// How long a client may take to accept a batch of responses before the
    /// connection is dropped (default: 30 s).
    pub fn write_timeout(mut self, timeout: Duration) -> Self {
        self.cfg.write_timeout = timeout;
        self
    }

    /// Largest accepted request head, in bytes (default: 64 KiB). Larger heads
    /// get `431 Request Header Fields Too Large`.
    pub fn max_head_size(mut self, bytes: usize) -> Self {
        self.cfg.max_head_size = bytes.max(64);
        self
    }

    /// Largest accepted request body, in bytes (default: 1 MiB). Larger bodies
    /// get `413 Content Too Large`.
    pub fn max_body_size(mut self, bytes: usize) -> Self {
        self.cfg.max_body_size = bytes;
        self
    }

    /// After a shutdown is requested, how long the server keeps picking up
    /// connections and requests that were already on their way (default:
    /// 100 ms). Requests in progress are not affected by this: they get the
    /// runtime's [`shutdown_grace`](giong::Builder::shutdown_grace).
    pub fn shutdown_linger(mut self, linger: Duration) -> Self {
        self.cfg.shutdown_linger = linger;
        self
    }

    /// Most connections one [`serve`](Self::serve) call keeps open at once
    /// (default: no limit). Normally every worker makes its own call, so the
    /// limit applies per worker.
    ///
    /// At the limit the server stops accepting until a connection closes.
    /// New clients wait in the kernel's listen queue meanwhile instead of
    /// being refused, and the server never runs out of file descriptors or
    /// memory because of the number of clients. Connections handed to
    /// [`serve_connection`](Self::serve_connection) are not counted.
    pub fn max_connections(mut self, max: usize) -> Self {
        self.cfg.max_connections = max.max(1);
        self
    }

    /// Accepts connections on `listener` and answers their requests with
    /// `handler` until the runtime is asked to shut down.
    ///
    /// Each connection runs as its own task on the current worker. When a
    /// shutdown is requested, the listener stops accepting, idle connections
    /// are closed, and requests being handled are finished and answered with
    /// `Connection: close`, within the grace period configured on the runtime.
    ///
    /// A handler that panics takes down its connection only.
    pub async fn serve<H: Handler>(&self, listener: TcpListener, handler: H) {
        let handler = Rc::new(handler);
        let cfg = self.cfg;
        let accept = || listener.accept();
        self.accept_loop(accept, |stream: TcpStream, slot| {
            let _ = stream.set_nodelay(true);
            drop(giong::spawn(
                Conn::new(stream, cfg, Some(slot)).serve(handler.clone()),
            ));
        })
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
    pub async fn serve_unix<H: Handler>(&self, listener: UnixListener, handler: H) {
        let handler = Rc::new(handler);
        let cfg = self.cfg;
        let accept = || listener.accept();
        self.accept_loop(accept, |stream: UnixStream, slot| {
            drop(giong::spawn(
                Conn::new(stream, cfg, Some(slot)).serve(handler.clone()),
            ));
        })
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
    /// let acceptor = giong_tls::TlsAcceptor::from_pem(
    ///     &std::fs::read("cert.pem")?,
    ///     &std::fs::read("key.pem")?,
    /// )?;
    /// let listener = TcpListener::bind("0.0.0.0:8443")?;
    /// Server::new().serve_tls(listener, acceptor, handler).await;
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "tls")]
    pub async fn serve_tls<H: Handler>(
        &self,
        listener: TcpListener,
        acceptor: giong_tls::TlsAcceptor,
        handler: H,
    ) {
        let handler = Rc::new(handler);
        let cfg = self.cfg;
        let accept = || listener.accept();
        self.accept_loop(accept, |stream: TcpStream, slot| {
            let _ = stream.set_nodelay(true);
            let (acceptor, handler) = (acceptor.clone(), handler.clone());
            drop(giong::spawn(async move {
                // A failed handshake (a port scan, a plain-HTTP client, an
                // unknown protocol version) just ends the connection.
                if let Ok(tls) = acceptor.accept(stream).await {
                    Conn::new(tls, cfg, Some(slot)).serve(handler).await;
                }
            }));
        })
        .await;
    }

    /// Accepts until a shutdown is requested, then for a moment longer.
    /// Waits for a free slot before each accept when `max_connections` is set.
    async fn accept_loop<S, A>(&self, accept: impl Fn() -> A, spawn: impl Fn(S, Slot))
    where
        A: Future<Output = io::Result<S>>,
    {
        let slots = Rc::new(Slots::new(self.cfg.max_connections));
        // Cancel safe: a slot taken by a dropped future is given back, and
        // `accept` itself loses no connection.
        let next = || async {
            let slot = slots.take().await;
            accept().await.map(|stream| (stream, slot))
        };
        while let Some(accepted) = giong::until_shutdown(next()).await {
            match accepted {
                Ok((stream, slot)) => spawn(stream, slot),
                // For example EMFILE: back off instead of spinning.
                Err(_) => sleep(Duration::from_millis(50)).await,
            }
        }
        // Connections established just before the shutdown still get served.
        let deadline = Instant::now() + self.cfg.shutdown_linger;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match timeout(left, next()).await {
                Ok(Ok((stream, slot))) => spawn(stream, slot),
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
        Conn::new(stream, self.cfg, None).serve(handler)
    }
}

/// The open connections of one accept loop, against its limit. Only the
/// accept loop waits for a slot, so one waker is enough.
struct Slots {
    open: Cell<usize>,
    max: usize,
    waiter: Cell<Option<Waker>>,
}

impl Slots {
    fn new(max: usize) -> Self {
        Self {
            open: Cell::new(0),
            max,
            waiter: Cell::new(None),
        }
    }

    /// Waits until fewer than `max` connections are open, then takes a slot.
    async fn take(self: &Rc<Self>) -> Slot {
        poll_fn(|cx| {
            if self.open.get() < self.max {
                self.open.set(self.open.get() + 1);
                Poll::Ready(Slot(self.clone()))
            } else {
                self.waiter.set(Some(cx.waker().clone()));
                Poll::Pending
            }
        })
        .await
    }
}

/// One open connection; frees its slot when dropped.
struct Slot(Rc<Slots>);

impl Drop for Slot {
    fn drop(&mut self) {
        let slots = &self.0;
        slots.open.set(slots.open.get() - 1);
        if let Some(waker) = slots.waiter.take() {
            waker.wake();
        }
    }
}

/// Why a connection stops reading requests.
enum End {
    /// Close without a response: end of stream, idle timeout, I/O error.
    Close,
    /// Answer with this error status, then close.
    Status(u16),
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
    cfg: Config,
    /// The client's address, looked up once per connection.
    peer: Option<SocketAddr>,
    /// Received bytes. Shared with the requests handed to the handler, so it
    /// is only modified in place once those have been dropped.
    buf: Rc<Vec<u8>>,
    /// Where the unparsed data starts in `buf`.
    pos: usize,
    /// How much of the unparsed data has been searched for the end of a head.
    scanned: usize,
    /// Responses not sent yet.
    out: Vec<u8>,
    /// A large body to send right after `out`, which ends with its head.
    body: Body,
    /// The server is shutting down: answer what arrives, then close.
    closing: bool,
    /// Deadline for the request currently being received.
    deadline: Option<Instant>,
    /// The connection's place under `max_connections`, given back when the
    /// connection is dropped. Declared last, so that the stream is closed
    /// first.
    _slot: Option<Slot>,
}

impl<T: Transport> Conn<T> {
    fn new(stream: T, cfg: Config, slot: Option<Slot>) -> Self {
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
            _slot: slot,
        }
    }

    /// The connection's task.
    ///
    /// The connection lives in this future and [`run`](Self::run) borrows
    /// it there. An `async fn` that took it by value would keep it twice
    /// (the argument, and the variable it is moved into), and every idle
    /// connection would pay for the copy.
    fn serve<H: Handler>(self, handler: Rc<H>) -> impl Future<Output = ()> {
        let mut conn = self;
        async move { conn.run(&*handler).await }
    }

    async fn run<H: Handler>(&mut self, handler: &H) {
        loop {
            let req = match self.next_request().await {
                Ok(req) => req,
                Err(End::Close) => break,
                Err(End::Status(status)) => {
                    self.reserve_out();
                    Response::new(status).write_to(&mut self.out, 1, false, false);
                    if self.flush().await.is_ok() {
                        self.lingering_close().await;
                    }
                    return;
                }
            };
            let minor = req.head.minor;
            let head_only = req.method() == "HEAD";
            let wants_keep_alive = !req.head.close && (minor == 1 || req.head.keep_alive);

            // Poll the handler once before deciding what to do with responses
            // that are waiting: if it answers at once (the common case), they
            // all go out in one write; if it has to wait, they go out first.
            //
            // The handler's future lives on the heap for the length of one
            // request. Kept inside the connection's own future, it would make
            // every connection as large as the largest handler future (a
            // database query, a call to another service) even while idle.
            let mut fut = HeapFuture::new(handler.call(req));
            let first = poll_fn(|cx| Poll::Ready(Pin::new(&mut fut).poll(cx))).await;
            let resp = match first {
                Poll::Ready(resp) => resp,
                Poll::Pending => {
                    if self.flush().await.is_err() {
                        return;
                    }
                    fut.await
                }
            };

            // Decided after the handler ran: a shutdown may have begun meanwhile.
            self.closing |= giong::is_shutting_down();
            let keep_alive = wants_keep_alive && !self.closing;
            self.reserve_out();
            let mut resp = resp;
            if resp.sends_body(head_only) && resp.body_len() >= SEPARATE_BODY_AT {
                // Sent from where it is, right after its head.
                resp.write_head(&mut self.out, minor, keep_alive);
                self.body = resp.take_body();
            } else {
                resp.write_to(&mut self.out, minor, keep_alive, head_only);
            }
            if !keep_alive {
                break;
            }
            // Nothing may be appended after a separate body: send it now.
            let send_now = self.out.len() >= FLUSH_AT || !matches!(self.body, Body::Empty);
            if send_now && self.flush().await.is_err() {
                return;
            }
        }
        if self.flush().await.is_ok() {
            self.close_write().await;
        }
    }

    /// Receives and parses the next request, body included.
    async fn next_request(&mut self) -> Result<Request, End> {
        let mut blank_lines = 0;
        let head = loop {
            // Empty lines before a request line are ignored (RFC 9112 §2.2).
            while self.buf[self.pos..].starts_with(b"\r\n") {
                self.pos += 2;
                self.scanned = 0;
                blank_lines += 1;
            }
            let data = &self.buf[self.pos..];
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

        let mut interim_sent = false;
        let (body, total) = if head.chunked {
            let mut decoder = Decoder::new(self.cfg.max_body_size);
            loop {
                let data = &self.buf[self.pos + head.len..];
                match decoder.feed(data).map_err(End::Status)? {
                    Progress::Done(used) => {
                        break (
                            BodyBytes::Decoded(mem::take(&mut decoder.body)),
                            head.len + used,
                        )
                    }
                    Progress::NeedMore => {
                        self.send_continue(&head, &mut interim_sent);
                        self.read_more(false).await?;
                    }
                }
            }
        } else {
            let len = head.content_length.unwrap_or(0);
            if len > self.cfg.max_body_size as u64 {
                return Err(End::Status(413));
            }
            let len = len as usize;
            while self.buf.len() - self.pos - head.len < len {
                self.send_continue(&head, &mut interim_sent);
                self.read_more(false).await?;
            }
            let start = self.pos + head.len;
            (BodyBytes::InBuffer(start..start + len), head.len + len)
        };

        let req = Request {
            buf: self.buf.clone(),
            start: self.pos,
            head,
            body,
            peer: self.peer,
        };
        self.pos += total;
        self.scanned = 0;
        self.deadline = None;
        Ok(req)
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
        let received = if idle {
            if self.closing {
                return Err(End::Close);
            }
            give_buffer(mem::take(&mut self.out));
            let recv = timeout(self.cfg.idle_timeout, self.stream.recv());
            match giong::until_shutdown(recv).await {
                Some(received) => received,
                None => {
                    // Shutting down: a request that was already on its way is
                    // still answered, then the connection closes.
                    self.closing = true;
                    timeout(self.cfg.shutdown_linger, self.stream.recv()).await
                }
            }
        } else {
            let request_timeout = self.cfg.request_timeout;
            let deadline = *self
                .deadline
                .get_or_insert_with(|| Instant::now() + request_timeout);
            let left = deadline.saturating_duration_since(Instant::now());
            match timeout(left, self.stream.recv()).await {
                Ok(received) => Ok(received),
                Err(_) => return Err(End::Status(408)),
            }
        };
        match received {
            Ok(Ok(chunk)) if !chunk.is_empty() => {
                self.append(&chunk);
                Ok(())
            }
            // Idle for too long, an I/O error, or closed by the peer.
            _ => Err(End::Close),
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

    fn reserve_out(&mut self) {
        if self.out.capacity() == 0 {
            self.out = take_buffer();
        }
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
        }
    }

    /// Writes everything in `out`. One send per batch of responses.
    async fn flush(&mut self) -> Result<(), End> {
        if self.out.is_empty() {
            return Ok(());
        }
        let out = mem::take(&mut self.out);
        let wrote = match mem::take(&mut self.body) {
            Body::Empty => timeout(self.cfg.write_timeout, self.stream.write_all(out)).await,
            body => {
                let write = self.stream.write_all_with_body(out, body);
                timeout(self.cfg.write_timeout, write).await
            }
        };
        match wrote {
            Ok((Ok(()), mut out)) => {
                out.clear();
                self.out = out;
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
                let conn = Conn::new(stream, Server::new().cfg, None).serve(Rc::new(handler));
                let size = mem::size_of_val(&conn);
                assert!(size <= 2048, "a connection's future takes {size} bytes");
            })
            .expect("runtime");
    }
}
