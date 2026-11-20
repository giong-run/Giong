//! The server side: [`Server`] answers gRPC calls as a giong-http handler,
//! and the generated service code runs each call through [`Call`].

use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::{poll_fn, Future};
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use giong::error::Code as ErrorCode;
use giong::time::{sleep, sleep_until};
use giong_http::{Body, BodySender, BodyStream, Trailers, Version};
use giong_proto::{MessageOf, MessageType, View};

use crate::codec::{self, Deframer, Payload};
use crate::message::{Received, Reply};
use crate::metadata::Metadata;
use crate::status::{Code, Status};

/// The largest message a server or a channel reads or writes unless told
/// otherwise: 4 MiB, as in other gRPC implementations.
pub(crate) const DEFAULT_MAX_MESSAGE: usize = 4 << 20;

/// A gRPC service a [`Server`] can route calls to. Implemented by the
/// `...Server` type giong-grpc-build generates for each service; there is
/// no reason to implement it by hand.
pub trait Service: 'static {
    /// The service's full name: `package.Service`.
    fn name(&self) -> &'static str;

    /// Runs the method named `method` (the last part of the call's path)
    /// through one of [`Call`]'s four methods, or [`Call::unimplemented`]
    /// for a name the service does not have.
    fn call(&self, method: &str, call: Call) -> CallFuture;
}

/// What [`Service::call`] returns: the call, running.
pub type CallFuture = Pin<Box<dyn Future<Output = Answer>>>;

/// The HTTP response a call produced. Made by [`Call`]'s methods.
pub struct Answer(giong_http::Response);

impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Answer")
    }
}

/// A gRPC server: the services it offers and its limits. It is a
/// [`giong_http::Handler`], served by giong-http over HTTP/2:
///
/// ```ignore
/// let grpc = giong_grpc::Server::new().add(OrdersServer::new(MyOrders));
/// let listener = giong::net::TcpListener::bind("127.0.0.1:50051")?;
/// giong_http::Server::new().serve(listener, grpc).await;
/// ```
///
/// With TLS, `serve_tls` and a certificate: clients choose HTTP/2 through
/// ALPN. Without, clients speak HTTP/2 with prior knowledge, which
/// giong-http accepts on the same port.
///
/// What a call may take is bounded on both sides: each message by
/// [`max_message_size`](Self::max_message_size), the whole call by the
/// deadline the client sent (`grpc-timeout`) or
/// [`default_timeout`](Self::default_timeout), and the silence of a client
/// between two messages of a streaming call by giong-http's
/// [`request_timeout`](giong_http::Server::request_timeout).
#[derive(Clone)]
pub struct Server {
    inner: Rc<Inner>,
}

struct Inner {
    services: Vec<Rc<dyn Service>>,
    limits: Limits,
}

#[derive(Clone, Copy)]
struct Limits {
    max_message: usize,
    compress: bool,
    default_timeout: Duration,
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    /// A server without services: every call is answered `UNIMPLEMENTED`
    /// until [`add`](Self::add) gives it some.
    pub fn new() -> Self {
        Self {
            inner: Rc::new(Inner {
                services: Vec::new(),
                limits: Limits {
                    max_message: DEFAULT_MAX_MESSAGE,
                    compress: false,
                    default_timeout: Duration::from_secs(3600),
                },
            }),
        }
    }

    fn inner(&mut self) -> &mut Inner {
        if Rc::get_mut(&mut self.inner).is_none() {
            let copy = Inner {
                services: self.inner.services.clone(),
                limits: self.inner.limits,
            };
            self.inner = Rc::new(copy);
        }
        Rc::get_mut(&mut self.inner).expect("just made unique")
    }

    /// Adds a service. A second service with the same name replaces the
    /// first.
    // Not `std::ops::Add`: a server is not the sum of two things.
    #[allow(clippy::should_implement_trait)]
    pub fn add(mut self, service: impl Service) -> Self {
        let inner = self.inner();
        inner.services.retain(|s| s.name() != service.name());
        inner.services.push(Rc::new(service));
        self
    }

    /// The largest message the server reads or writes, in bytes (4 MiB by
    /// default, at most 4 GiB less one byte). A larger one ends its call
    /// with `RESOURCE_EXHAUSTED` (`GIONG-E4102`). A compressed message
    /// counts by its size once decompressed.
    pub fn max_message_size(mut self, bytes: usize) -> Self {
        self.inner().limits.max_message = bytes.min(u32::MAX as usize);
        self
    }

    /// Whether responses are compressed with gzip for clients that accept
    /// it (`false` by default). One reply or stream can decide otherwise
    /// ([`Reply::compress`], [`Sink::compress`]).
    pub fn compress(mut self, on: bool) -> Self {
        self.inner().limits.compress = on;
        self
    }

    /// How long a call may take when its client sent no deadline (one hour
    /// by default). A call that takes longer ends with
    /// `DEADLINE_EXCEEDED`, and its handler is dropped.
    pub fn default_timeout(mut self, timeout: Duration) -> Self {
        self.inner().limits.default_timeout = timeout;
        self
    }

    /// The names of the services added, in the order they were added.
    pub fn service_names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.inner.services.iter().map(|s| s.name())
    }
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Server")
            .field("services", &self.service_names().collect::<Vec<_>>())
            .field("max_message_size", &self.inner.limits.max_message)
            .field("compress", &self.inner.limits.compress)
            .field("default_timeout", &self.inner.limits.default_timeout)
            .finish()
    }
}

impl giong_http::Handler for Server {
    fn stream_body(&self, _req: &giong_http::Request) -> Option<u64> {
        // Messages have their own limit; a streaming call has no total.
        Some(u64::MAX)
    }

    async fn call(&self, req: giong_http::Request) -> giong_http::Response {
        let limits = self.inner.limits;
        let content_type = req.header("content-type").unwrap_or_default();
        let is_grpc = content_type == b"application/grpc"
            || content_type.starts_with(b"application/grpc+")
            || content_type.starts_with(b"application/grpc;");
        if !is_grpc {
            // Not a gRPC client: plain HTTP is the only answer it reads.
            return giong_http::Response::new(415)
                .header("content-type", "text/plain; charset=utf-8")
                .header("accept-post", "application/grpc")
                .body("this is a gRPC server: requests need content-type application/grpc\n");
        }
        if req.version() != Version::Http2 || req.method() != "POST" {
            return trailers_only(&Status::raised(
                Code::Internal,
                ErrorCode::GrpcProtocol,
                format!(
                    "a gRPC call must be a POST over HTTP/2, and this was a {} over {}",
                    req.method(),
                    req.version().as_str()
                ),
                "connect with HTTP/2: through TLS with ALPN, or with prior knowledge on a \
                 plain connection.",
            ));
        }
        // "/package.Service/Method"
        let path = req.path();
        let (service, method) = match path.strip_prefix('/').and_then(|p| p.split_once('/')) {
            Some((service, method)) if !method.contains('/') => (service, method),
            _ => return trailers_only(&no_method(path)),
        };
        let Some(service) = self.inner.services.iter().find(|s| s.name() == service) else {
            return trailers_only(&no_method(path));
        };
        let call = match Call::new(&req, limits) {
            Ok(call) => call,
            Err(status) => return trailers_only(&status),
        };
        let method = method.to_owned();
        service.call(&method, call).await.0
    }
}

fn no_method(path: &str) -> Status {
    Status::raised(
        Code::Unimplemented,
        ErrorCode::GrpcUnimplemented,
        format!("this server has no method {path}"),
        "check the service and method names (and the package) against the .proto the \
         server was built from, and that the service was added to the giong_grpc::Server.",
    )
}

// ------------------------------------------------------------ responses

/// The fields that end a call: its status, and the metadata after it.
fn status_fields(status: &Status, extra: &Metadata) -> Vec<(String, Vec<u8>)> {
    let mut fields = Vec::with_capacity(3 + status.metadata().len() + extra.len());
    fields.push((
        "grpc-status".to_owned(),
        status.code().number().to_string().into_bytes(),
    ));
    if !status.message().is_empty() {
        fields.push((
            "grpc-message".to_owned(),
            codec::encode_message(status.message()).into_bytes(),
        ));
    }
    if !status.details().is_empty() {
        fields.push((
            "grpc-status-details-bin".to_owned(),
            crate::metadata::base64_encode(status.details()).into_bytes(),
        ));
    }
    for (name, value) in extra.wire().chain(status.metadata().wire()) {
        fields.push((name.to_owned(), value.as_bytes().to_vec()));
    }
    fields
}

/// The trailers that end a call. Metadata that cannot travel in trailers
/// fails the call instead, so that it is noticed.
fn closing(status: &Status, extra: &Metadata) -> Trailers {
    let mut trailers = Trailers::new();
    for (name, value) in status_fields(status, extra) {
        trailers = match trailers.field(&name, value) {
            Ok(trailers) => trailers,
            Err(e) => return closing(&bad_trailer(&name, &e), &Metadata::new()),
        };
    }
    trailers
}

fn bad_trailer(name: &str, error: &giong::error::Error) -> Status {
    Status::raised(
        Code::Internal,
        ErrorCode::GrpcInvalidMetadata,
        format!("the metadata name {name} cannot be sent after the response: {error}"),
        "send it with Reply::with_metadata or Sink::set_metadata, before the messages, \
         or choose another name.",
    )
}

fn head(gzip: bool) -> giong_http::Response {
    let response = giong_http::Response::new(200)
        .header("content-type", "application/grpc")
        .header("grpc-accept-encoding", "identity,gzip");
    if gzip {
        response.header("grpc-encoding", "gzip")
    } else {
        response
    }
}

fn with_metadata(
    mut response: giong_http::Response,
    metadata: &Metadata,
) -> Result<giong_http::Response, Status> {
    for (name, value) in metadata.wire() {
        // Printable ASCII or base64: text.
        let value = std::str::from_utf8(value.as_bytes()).unwrap_or_default();
        response = response.try_header(name, value).map_err(|e| {
            Status::raised(
                Code::Internal,
                ErrorCode::GrpcInvalidMetadata,
                format!("the metadata name {name} cannot be sent: {e}"),
                "choose another name.",
            )
        })?;
    }
    Ok(response)
}

/// A call that ends without a message: the status travels in the headers
/// of a response without a body.
fn trailers_only(status: &Status) -> giong_http::Response {
    let mut response = head(false);
    for (name, value) in status_fields(status, &Metadata::new()) {
        let value = String::from_utf8_lossy(&value).into_owned();
        response = match response.try_header(&name, value) {
            Ok(response) => response,
            Err(e) => {
                let error = giong::error::Error::new(
                    ErrorCode::GrpcInvalidMetadata,
                    e.to_string(),
                    "choose another name.",
                );
                return trailers_only(&bad_trailer(&name, &error));
            }
        };
    }
    response
}

// ----------------------------------------------------------------- calls

/// One call as it reaches a service: what the generated code turns into a
/// call of the handler, through the method that matches the kind of the
/// rpc.
pub struct Call {
    body: Option<BodyStream>,
    metadata: Metadata,
    deadline: Instant,
    /// The client set the deadline (`grpc-timeout`).
    timeout: Option<Duration>,
    peer: Option<SocketAddr>,
    limits: Limits,
    /// The client compresses its messages with gzip.
    request_gzip: bool,
    /// The client reads gzip.
    accepts_gzip: bool,
}

impl Call {
    fn new(req: &giong_http::Request, limits: Limits) -> Result<Self, Status> {
        // `None` for a request that ended with its head: no messages.
        let body = req.body_stream();
        let timeout = match req.header("grpc-timeout") {
            None => None,
            Some(value) => Some(codec::parse_timeout(value).ok_or_else(|| {
                codec::protocol(format!(
                    "the grpc-timeout {:?} is not up to 8 digits and a unit (H, M, S, m, u or n)",
                    String::from_utf8_lossy(value)
                ))
            })?),
        };
        let request_gzip = match req.header("grpc-encoding") {
            None | Some(b"identity") => false,
            Some(b"gzip") => true,
            Some(other) => {
                return Err(Status::raised(
                    Code::Unimplemented,
                    ErrorCode::GrpcEncoding,
                    format!(
                        "the request's messages are compressed with {:?}, which this server \
                         does not have",
                        String::from_utf8_lossy(other)
                    ),
                    "send messages plain or with gzip (grpc-accept-encoding: identity,gzip).",
                ))
            }
        };
        let accepts_gzip = req
            .header("grpc-accept-encoding")
            .is_some_and(|v| v.split(|&b| b == b',').any(|e| trim(e) == b"gzip"));
        let mut metadata = Metadata::new();
        for (name, value) in req.headers() {
            metadata.received(name, value).map_err(|name| {
                codec::protocol(format!(
                    "the value of the metadata name {name} is not base64"
                ))
            })?;
        }
        Ok(Self {
            body,
            metadata,
            deadline: Instant::now() + timeout.unwrap_or(limits.default_timeout),
            timeout,
            peer: req.peer_addr(),
            limits,
            request_gzip,
            accepts_gzip,
        })
    }

    fn incoming<M: MessageType>(&mut self) -> RequestStream<M> {
        RequestStream {
            body: self.body.clone(),
            deframer: Deframer::new(self.limits.max_message, self.request_gzip),
            metadata: std::mem::take(&mut self.metadata),
            deadline: self.deadline,
            timeout: self.timeout,
            peer: self.peer,
            done: false,
            response: None,
            marker: PhantomData,
        }
    }

    fn gzip(&self, choice: Option<bool>) -> bool {
        self.accepts_gzip && choice.unwrap_or(self.limits.compress)
    }

    fn deadline_passed(&self) -> Status {
        deadline_passed(self.timeout.unwrap_or(self.limits.default_timeout))
    }

    /// Answers `UNIMPLEMENTED`: the service has no method of that name.
    pub fn unimplemented(self) -> CallFuture {
        Box::pin(async move {
            Answer(trailers_only(&Status::raised(
                Code::Unimplemented,
                ErrorCode::GrpcUnimplemented,
                "this service has no method of that name",
                "check the method name against the .proto the server was built from.",
            )))
        })
    }

    /// Runs a unary method: one request, one response.
    ///
    /// The handler runs inside the HTTP request: if the client cancels the
    /// call or its connection closes, the handler's future is dropped at
    /// its next `.await`. So is it when the deadline passes, and the call
    /// ends with `DEADLINE_EXCEEDED`.
    pub fn unary<Req, Resp, F, Fut>(mut self, handler: F) -> CallFuture
    where
        Req: MessageType,
        Resp: MessageType,
        F: FnOnce(Request<Req>) -> Fut + 'static,
        Fut: Future<Output = Result<Reply<Resp>, Status>> + 'static,
    {
        Box::pin(async move {
            let mut stream = self.incoming::<Req>();
            let run = async {
                let request = stream.single().await?;
                handler(request).await
            };
            let result = self.within_deadline(run).await;
            Answer(self.reply(result).await)
        })
    }

    /// Runs a client-streaming method: a stream of requests, one response.
    /// The handler runs inside the HTTP request, as for
    /// [`unary`](Self::unary).
    pub fn client_streaming<Req, Resp, F, Fut>(mut self, handler: F) -> CallFuture
    where
        Req: MessageType,
        Resp: MessageType,
        F: FnOnce(RequestStream<Req>) -> Fut + 'static,
        Fut: Future<Output = Result<Reply<Resp>, Status>> + 'static,
    {
        Box::pin(async move {
            let stream = self.incoming::<Req>();
            let result = self.within_deadline(handler(stream)).await;
            Answer(self.reply(result).await)
        })
    }

    /// Runs a server-streaming method: one request, a stream of responses.
    ///
    /// The handler runs in a task of its own, since the response starts
    /// before it ends. The task is dropped when the client cancels the
    /// call or goes away, and when the deadline passes.
    pub fn server_streaming<Req, Resp, F, Fut>(mut self, handler: F) -> CallFuture
    where
        Req: MessageType,
        Resp: MessageType,
        F: FnOnce(Request<Req>, Sink<Resp>) -> Fut + 'static,
        Fut: Future<Output = Result<(), Status>> + 'static,
    {
        Box::pin(async move {
            let mut stream = self.incoming::<Req>();
            let first = self.within_deadline(stream.single()).await;
            let request = match first {
                Ok(request) => request,
                Err(status) => return Answer(trailers_only(&status)),
            };
            Answer(self.stream(move |sink| handler(request, sink)).await)
        })
    }

    /// Runs a bidirectional method: streams both ways. The handler runs
    /// in a task of its own, as for
    /// [`server_streaming`](Self::server_streaming).
    ///
    /// The response starts (its headers are sent, with the metadata set
    /// until then) when the handler first sends a message or first reads
    /// a request, whichever comes first: a client that waits for the
    /// response to start before it sends its first request, as some do,
    /// is not left waiting.
    pub fn bidi<Req, Resp, F, Fut>(mut self, handler: F) -> CallFuture
    where
        Req: MessageType,
        Resp: MessageType,
        F: FnOnce(RequestStream<Req>, Sink<Resp>) -> Fut + 'static,
        Fut: Future<Output = Result<(), Status>> + 'static,
    {
        Box::pin(async move {
            let mut stream = self.incoming::<Req>();
            let start = move |sink: Sink<Resp>| {
                stream.response = Some(sink.shared.clone());
                handler(stream, sink)
            };
            Answer(self.stream(start).await)
        })
    }

    /// Runs `work` until the deadline, turning a panic into `INTERNAL`.
    async fn within_deadline<T>(
        &self,
        work: impl Future<Output = Result<T, Status>>,
    ) -> Result<T, Status> {
        let mut work = std::pin::pin!(work);
        let mut timer = std::pin::pin!(sleep_until(self.deadline));
        poll_fn(|cx| {
            match catch_unwind(AssertUnwindSafe(|| work.as_mut().poll(cx))) {
                Ok(Poll::Ready(result)) => return Poll::Ready(result),
                Ok(Poll::Pending) => {}
                Err(_) => return Poll::Ready(Err(panicked())),
            }
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(self.deadline_passed()));
            }
            Poll::Pending
        })
        .await
    }

    /// The response to a call that answers with one message.
    async fn reply<Resp: MessageType>(
        &self,
        result: Result<Reply<Resp>, Status>,
    ) -> giong_http::Response {
        let reply = match result {
            Ok(reply) => reply,
            Err(status) => return trailers_only(&status),
        };
        let (payload, metadata, trailers, compress) = reply.into_parts();
        let gzip = self.gzip(compress) && !payload.as_slice().is_empty();
        let frame = match payload.into_frame(gzip, self.limits.max_message) {
            Ok(frame) => frame,
            Err(status) => return trailers_only(&status),
        };
        let response = match with_metadata(head(gzip), &metadata) {
            Ok(response) => response,
            Err(status) => return trailers_only(&status),
        };
        let (sender, body) = Body::channel();
        // The pipe is empty, so the frame is taken at once.
        let _ = sender.send(frame).await;
        let _ = sender.send_trailers(closing(&Status::new(Code::Ok, ""), &trailers));
        response.body(body)
    }

    /// Starts the task of a call that answers with a stream, and waits
    /// until it has something to answer with: its first message, its
    /// metadata, or its end.
    async fn stream<Resp, Fut>(self, start: impl FnOnce(Sink<Resp>) -> Fut) -> giong_http::Response
    where
        Resp: MessageType,
        Fut: Future<Output = Result<(), Status>> + 'static,
    {
        let (sender, body) = Body::channel();
        let gzip = self.gzip(None);
        let shared = Rc::new(Shared {
            sender: RefCell::new(Some(Rc::new(sender))),
            started: Cell::new(false),
            metadata: RefCell::new(Metadata::new()),
            trailers: RefCell::new(Metadata::new()),
            head: RefCell::new(None),
            waiting: RefCell::new(None),
            gzip: Cell::new(gzip),
            announced: Cell::new(gzip),
            may_gzip: self.accepts_gzip,
            max_message: self.limits.max_message,
            pending: RefCell::new(Vec::new()),
            flusher: RefCell::new(None),
            space: RefCell::new(None),
            driving: Cell::new(false),
            closed: Cell::new(false),
        });
        let sink = Sink {
            shared: shared.clone(),
            marker: PhantomData,
        };
        let work = start(sink);
        let deadline = self.deadline;
        let late = self.deadline_passed();
        drop(giong::spawn(drive(shared.clone(), work, deadline, late)));

        // The body is held here until the head is known: if this future is
        // dropped (the client reset the stream), the task is told through
        // its sender.
        let head_ready = poll_fn(|cx| match shared.head.borrow_mut().take() {
            Some(head) => Poll::Ready(head),
            None => {
                *shared.waiting.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;
        match head_ready {
            Head::Streaming { metadata, gzip } => match with_metadata(head(gzip), &metadata) {
                Ok(response) => response.body(body),
                Err(status) => trailers_only(&status),
            },
            Head::Ended(status) => trailers_only(&status),
        }
    }
}

impl fmt::Debug for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Call")
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("peer", &self.peer)
            .finish()
    }
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

fn panicked() -> Status {
    Status::new(Code::Internal, "the handler of this method panicked")
}

fn deadline_passed(timeout: Duration) -> Status {
    Status::raised(
        Code::DeadlineExceeded,
        ErrorCode::GrpcDeadline,
        format!("the call did not end within its deadline of {timeout:?}"),
        "give the call more time (the client's timeout, or Server::default_timeout when \
         the client sends none), or make the handler faster.",
    )
}

fn cancelled(what: &str) -> Status {
    Status::raised(
        Code::Cancelled,
        ErrorCode::GrpcCancelled,
        what,
        "nothing to fix on this side: the other end gave up on the call; stop working \
         on it.",
    )
}

/// How the response to a streaming call starts.
enum Head {
    /// With headers; messages follow.
    Streaming { metadata: Metadata, gzip: bool },
    /// The call ended before it sent anything.
    Ended(Status),
}

/// What a streaming call's handler, its task and the HTTP request share.
struct Shared {
    /// Taken by the task when the call ends.
    sender: RefCell<Option<Rc<BodySender>>>,
    /// The head of the response is decided.
    started: Cell<bool>,
    metadata: RefCell<Metadata>,
    trailers: RefCell<Metadata>,
    head: RefCell<Option<Head>>,
    /// The HTTP request, waiting for the head.
    waiting: RefCell<Option<Waker>>,
    /// Whether the next message is compressed.
    gzip: Cell<bool>,
    /// The head of the response names gzip (or will), so messages may use
    /// it.
    announced: Cell<bool>,
    may_gzip: bool,
    max_message: usize,
    /// Messages sent and not yet handed to the connection, as frames one
    /// after the other: the call's task hands them over together.
    pending: RefCell<Vec<u8>>,
    /// The call's task, woken when a sink in another task has sent.
    flusher: RefCell<Option<Waker>>,
    /// A send waiting for `pending` to be handed over.
    space: RefCell<Option<Waker>>,
    /// The call's task is polling the handler: it looks at `pending`
    /// right after, so a send need not wake it.
    driving: Cell<bool>,
    /// The connection takes no more messages.
    closed: Cell<bool>,
}

/// Messages a streaming call has sent wait for the connection together up
/// to this many bytes; with more waiting, `Sink::send` waits.
const SEND_BUFFER: usize = 16 * 1024;

impl Shared {
    fn start(&self, head: Head) {
        self.started.set(true);
        *self.head.borrow_mut() = Some(head);
        if let Some(waker) = self.waiting.borrow_mut().take() {
            waker.wake();
        }
    }

    fn start_streaming(&self) {
        if !self.started.get() {
            let metadata = std::mem::take(&mut *self.metadata.borrow_mut());
            self.start(Head::Streaming {
                metadata,
                gzip: self.announced.get(),
            });
        }
    }
}

/// The task of a streaming call: runs the handler until it ends, the
/// deadline passes or the client goes away, then ends the response.
async fn drive<Fut>(shared: Rc<Shared>, work: Fut, deadline: Instant, late: Status)
where
    Fut: Future<Output = Result<(), Status>>,
{
    let Some(sender) = shared.sender.borrow().clone() else {
        return;
    };
    let result = {
        let mut work = std::pin::pin!(work);
        let mut timer = std::pin::pin!(sleep_until(deadline));
        let mut gone = std::pin::pin!(sender.closed());
        // The messages on their way to the connection.
        let mut flush = std::pin::pin!(None);
        let result = poll_fn(|cx: &mut Context<'_>| {
            if gone.as_mut().poll(cx).is_ready() {
                // Nobody is listening: the handler is dropped.
                return Poll::Ready(None);
            }
            {
                let mut flusher = shared.flusher.borrow_mut();
                if !flusher.as_ref().is_some_and(|w| w.will_wake(cx.waker())) {
                    *flusher = Some(cx.waker().clone());
                }
            }
            loop {
                shared.driving.set(true);
                let polled = catch_unwind(AssertUnwindSafe(|| work.as_mut().poll(cx)));
                shared.driving.set(false);
                match polled {
                    Ok(Poll::Ready(result)) => return Poll::Ready(Some(result)),
                    Ok(Poll::Pending) => {}
                    Err(_) => return Poll::Ready(Some(Err(panicked()))),
                }
                // The handler waits: what it sent goes to the connection,
                // every message since the last time in one piece.
                let mut handed = false;
                loop {
                    if flush.is_none() {
                        let mut pending = shared.pending.borrow_mut();
                        if pending.is_empty() {
                            break;
                        }
                        let piece = std::mem::take(&mut *pending);
                        flush.set(Some(sender.send(piece)));
                        handed = true;
                    }
                    let Some(sending) = flush.as_mut().as_pin_mut() else {
                        break;
                    };
                    match sending.poll(cx) {
                        Poll::Ready(Ok(())) => flush.set(None),
                        Poll::Ready(Err(_)) => {
                            flush.set(None);
                            shared.closed.set(true);
                            handed = true;
                            break;
                        }
                        Poll::Pending => break,
                    }
                }
                // A send that waited for room can go on: in the handler,
                // polled again here, or in another task.
                let waiter = shared.space.borrow_mut().take();
                match waiter {
                    Some(waiter) if handed => {
                        if !waiter.will_wake(cx.waker()) {
                            waiter.wake();
                        }
                    }
                    Some(waiter) => {
                        *shared.space.borrow_mut() = Some(waiter);
                        break;
                    }
                    None => break,
                }
            }
            if timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Some(Err(late.clone())));
            }
            Poll::Pending
        })
        .await;
        if result.is_some() && !shared.closed.get() {
            // What the handler sent last goes out before the status.
            let mut open = true;
            if let Some(sending) = flush.as_mut().as_pin_mut() {
                open = sending.await.is_ok();
            }
            flush.set(None);
            let rest = std::mem::take(&mut *shared.pending.borrow_mut());
            if open && !rest.is_empty() {
                let _ = sender.send(rest).await;
            }
        }
        result
    };
    shared.closed.set(true);
    if let Some(waiter) = shared.space.borrow_mut().take() {
        waiter.wake();
    }
    drop(sender);
    // From here on a sink the handler moved elsewhere cannot send.
    let taken = shared.sender.borrow_mut().take();
    let Some(result) = result else {
        return;
    };
    let status = match result {
        Ok(()) => Status::new(Code::Ok, ""),
        Err(status) => status,
    };
    if !shared.started.get() {
        if status.code() != Code::Ok || !shared.trailers.borrow().is_empty() {
            // Nothing was sent: the status is the whole response.
            let mut status = status;
            let trailers = std::mem::take(&mut *shared.trailers.borrow_mut());
            status.metadata_mut().extend(&trailers);
            shared.start(Head::Ended(status));
            return;
        }
        shared.start_streaming();
    }
    let Some(mut sender) = taken else {
        return;
    };
    // A send from a sink that was moved to another task may still be in
    // flight; it ends when the connection takes its piece or closes.
    let sender = loop {
        match Rc::try_unwrap(sender) {
            Ok(sender) => break sender,
            Err(shared_still) => {
                sender = shared_still;
                sleep(Duration::from_millis(1)).await;
            }
        }
    };
    let trailers = std::mem::take(&mut *shared.trailers.borrow_mut());
    let _ = sender.send_trailers(closing(&status, &trailers));
}

/// The responses of a streaming call: the handler sends each message with
/// [`send`](Self::send), and ends the call by returning.
///
/// Messages go out in order. The ones sent before the handler next waits
/// for something leave together (up to 16 KiB of them, then
/// [`send`](Self::send) waits until the connection has taken those), so a
/// handler that sends many small messages in a row pays for one write, and
/// a slow client slows the handler down instead of filling memory. Nothing
/// is held back for more to come: when the handler waits, what it sent is
/// on its way.
pub struct Sink<M> {
    shared: Rc<Shared>,
    marker: PhantomData<fn(M)>,
}

impl<M: MessageType> Sink<M> {
    /// Sends one message: it is on its way when this returns, and leaves
    /// with the others sent before the handler next waits.
    ///
    /// Cancel safe: if the future is dropped before it returns, the
    /// message was not sent.
    ///
    /// # Errors
    ///
    /// `CANCELLED` (`GIONG-E4109`) when the client has cancelled the call
    /// or gone away, or the call has ended: stop and return. A client that
    /// goes away while messages are on their way shows in the next `send`.
    /// `RESOURCE_EXHAUSTED` (`GIONG-E4102`) for a message past the
    /// server's limit; the call can go on with smaller ones.
    pub async fn send(&mut self, message: &impl MessageOf<Type = M>) -> Result<(), Status> {
        let shared = &self.shared;
        if message.encoded_len() > shared.max_message {
            return Err(codec::too_large(
                "the message to send",
                message.encoded_len(),
                shared.max_message,
            ));
        }
        poll_fn(|cx| {
            if shared.sender.borrow().is_none() {
                return Poll::Ready(Err(cancelled(
                    "the call has ended, so nothing more can be sent on it",
                )));
            }
            if shared.closed.get() {
                return Poll::Ready(Err(cancelled(
                    "the client cancelled the call or its connection closed",
                )));
            }
            if shared.pending.borrow().len() >= SEND_BUFFER {
                *shared.space.borrow_mut() = Some(cx.waker().clone());
                return Poll::Pending;
            }
            Poll::Ready(Ok(()))
        })
        .await?;
        codec::encode_frame_into(
            &mut shared.pending.borrow_mut(),
            message,
            shared.gzip.get(),
            shared.max_message,
        )?;
        // The head goes out with the first message, with the metadata set
        // until now.
        shared.start_streaming();
        if !shared.driving.get() {
            // Sent from another task than the call's: that one hands the
            // message to the connection.
            if let Some(flusher) = shared.flusher.borrow().as_ref() {
                flusher.wake_by_ref();
            }
        }
        Ok(())
    }

    /// Sets the metadata sent before the first message (the response's
    /// headers).
    ///
    /// # Errors
    ///
    /// `FAILED_PRECONDITION` once the response has started (a message was
    /// sent or, in a bidirectional call, a request was read): the headers
    /// are gone by then.
    pub fn set_metadata(&mut self, metadata: Metadata) -> Result<(), Status> {
        if self.shared.started.get() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "the response has started, so its headers are already sent; set the metadata \
                 before the first Sink::send and, in a bidirectional call, before reading \
                 the first request",
            ));
        }
        *self.shared.metadata.borrow_mut() = metadata;
        Ok(())
    }

    /// Sets the metadata sent after the last message, with the status
    /// (the response's trailers). The last call before the handler returns
    /// counts.
    pub fn set_trailers(&mut self, trailers: Metadata) {
        *self.shared.trailers.borrow_mut() = trailers;
    }

    /// Whether the messages sent from now on are compressed with gzip,
    /// instead of what the server is set to. A client that does not accept
    /// gzip gets them plain either way. It can change from one message to
    /// the next.
    ///
    /// # Errors
    ///
    /// `FAILED_PRECONDITION` when turning it on after the first message of
    /// a response that started without compression: the response's
    /// headers, which name the compression, are gone by then. Turn it on
    /// before the first [`send`](Self::send), then off and on as needed.
    pub fn compress(&mut self, on: bool) -> Result<(), Status> {
        let shared = &self.shared;
        if on && shared.may_gzip && shared.started.get() && !shared.announced.get() {
            return Err(Status::new(
                Code::FailedPrecondition,
                "the response started without compression, so its messages cannot be \
                 compressed now; call Sink::compress(true) before the first Sink::send",
            ));
        }
        let on = on && shared.may_gzip;
        if on {
            shared.announced.set(true);
        }
        shared.gzip.set(on);
        Ok(())
    }

    /// `true` once nothing more can be sent: the client cancelled the call
    /// or went away, or the call ended.
    pub fn is_closed(&self) -> bool {
        self.shared
            .sender
            .borrow()
            .as_ref()
            .map_or(true, |s| s.is_closed())
    }
}

impl<M> fmt::Debug for Sink<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sink")
            .field("started", &self.shared.started.get())
            .finish()
    }
}

/// The request of a unary or server-streaming call: its one message, and
/// what came with it.
pub struct Request<M> {
    payload: Payload,
    metadata: Metadata,
    deadline: Instant,
    timeout: Option<Duration>,
    peer: Option<SocketAddr>,
    marker: PhantomData<fn() -> M>,
}

impl<M: MessageType> Request<M> {
    /// Decodes the message, checking all of it. The view borrows from the
    /// request: strings, bytes and repeated fields are not copied.
    ///
    /// # Errors
    ///
    /// `INTERNAL` with `GIONG-E4105` if the bytes are not a valid message
    /// of this type: return it with `?`.
    pub fn message(&self) -> Result<M::View<'_>, Status> {
        Ok(M::View::decode(self.payload.as_slice())?)
    }

    /// The message's bytes, as protobuf encoded them.
    pub fn as_bytes(&self) -> &[u8] {
        self.payload.as_slice()
    }

    /// Whether the message arrived compressed.
    pub fn was_compressed(&self) -> bool {
        self.payload.was_compressed()
    }

    /// The metadata the client sent.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// When the call must have ended: after that the handler is dropped
    /// and the client gets `DEADLINE_EXCEEDED`. From the client's
    /// `grpc-timeout`, or the server's default.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The timeout the client sent, if it sent one.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// The client's address, when the connection has one.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer
    }
}

impl<M> fmt::Debug for Request<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("bytes", &self.payload.as_slice().len())
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("peer", &self.peer)
            .finish()
    }
}

/// The requests of a client-streaming or bidirectional call, read one at
/// a time, and what came with the call.
pub struct RequestStream<M> {
    body: Option<BodyStream>,
    deframer: Deframer,
    metadata: Metadata,
    deadline: Instant,
    timeout: Option<Duration>,
    peer: Option<SocketAddr>,
    done: bool,
    /// The response of a bidirectional call, which starts when the
    /// requests are first read.
    response: Option<Rc<Shared>>,
    marker: PhantomData<fn() -> M>,
}

impl<M: MessageType> RequestStream<M> {
    /// The next message, or `None` once the client has sent its last.
    ///
    /// In a bidirectional call the first call starts the response: its
    /// headers are sent, with the metadata set until then
    /// ([`Sink::set_metadata`]).
    ///
    /// Cancel safe: a message is only taken when it is returned.
    ///
    /// # Errors
    ///
    /// `CANCELLED` (`GIONG-E4109`) if the client cancelled the call or its
    /// connection closed, `RESOURCE_EXHAUSTED` (`GIONG-E4102`) for a
    /// message past the server's limit, `INTERNAL` (`GIONG-E4103`) for a
    /// stream that is not gRPC framing. After an error the stream is over.
    pub async fn message(&mut self) -> Result<Option<Received<M>>, Status> {
        loop {
            if self.done {
                return Ok(None);
            }
            match self.deframer.next() {
                Ok(Some(payload)) => return Ok(Some(Received::new(payload))),
                Ok(None) => {}
                Err(status) => {
                    self.done = true;
                    return Err(status);
                }
            }
            if let Some(response) = self.response.take() {
                response.start_streaming();
            }
            let piece = match &mut self.body {
                Some(body) => body.chunk().await,
                None => Ok(None),
            };
            match piece {
                Ok(Some(piece)) => self.deframer.push(piece),
                Ok(None) => {
                    self.done = true;
                    self.deframer.finish()?;
                }
                Err(e) => {
                    self.done = true;
                    return Err(match giong::error::code(&e) {
                        Some(giong::error::Code::HttpBodyIncomplete) => cancelled(
                            "the client cancelled the call or stopped sending before the \
                             end of its requests",
                        ),
                        _ => codec::protocol(format!("the request stream failed: {e}")),
                    });
                }
            }
        }
    }

    /// The metadata the client sent.
    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    /// When the call must have ended (see [`Request::deadline`]).
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// The timeout the client sent, if it sent one.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// The client's address, when the connection has one.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// Exactly one message, for the methods that take one request.
    async fn single(&mut self) -> Result<Request<M>, Status> {
        let wrong = |what: &str| {
            Status::raised(
                Code::Unimplemented,
                ErrorCode::GrpcProtocol,
                format!("this method takes one request message, and the client sent {what}"),
                "call the method as the .proto declares it: without `stream` in front of \
                 its request type, it takes exactly one message.",
            )
        };
        let Some(first) = self.message().await? else {
            return Err(wrong("none"));
        };
        if self.message().await?.is_some() {
            return Err(wrong("more than one"));
        }
        Ok(Request {
            payload: Payload::from(first),
            metadata: std::mem::take(&mut self.metadata),
            deadline: self.deadline,
            timeout: self.timeout,
            peer: self.peer,
            marker: PhantomData,
        })
    }
}

impl<M> fmt::Debug for RequestStream<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestStream")
            .field("metadata", &self.metadata)
            .field("timeout", &self.timeout)
            .field("peer", &self.peer)
            .field("done", &self.done)
            .finish()
    }
}

impl<M> From<Received<M>> for Payload {
    fn from(received: Received<M>) -> Self {
        received.into_payload()
    }
}
