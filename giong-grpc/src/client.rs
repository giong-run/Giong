//! The client side: a [`Channel`] to a server, which the generated
//! `...Client` types make their calls on.

use std::cell::Cell;
use std::fmt;
use std::future::{poll_fn, Future};
use std::marker::PhantomData;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, Instant};

use giong::error::{Code as ErrorCode, Error};
use giong::time::timeout;
use giong::JoinHandle;
use giong_http::client::{Client, ErrorKind, RequestBody, RequestBuilder, StreamingResponse};
use giong_http::Version;
use giong_proto::{MessageOf, MessageType};
use giong_tls::TlsConnector;

use crate::codec::{self, Deframer};
use crate::message::{Received, Reply};
use crate::metadata::Metadata;
use crate::server::DEFAULT_MAX_MESSAGE;
use crate::status::{Code, Status};

/// A connection to a gRPC server, for the calls of any number of generated
/// clients. Cloning gives another handle to the same one.
///
/// ```ignore
/// let channel = giong_grpc::Channel::new("http://127.0.0.1:50051")?;
/// let orders = OrdersClient::new(channel.clone());
/// let reply = orders
///     .get_order(&GetOrderRequest { id: 7, ..Default::default() }, CallOptions::new())
///     .await?;
/// println!("{}", reply.message()?.customer());
/// ```
///
/// An `http` address is spoken to in HTTP/2 with prior knowledge; an
/// `https` address through TLS, where the server must choose HTTP/2
/// (ALPN). The connection is opened by the first call and shared by every
/// call after it, as many at once as the server allows; another is opened
/// when it allows no more. A channel belongs to the worker it was made on.
///
/// Every call has a deadline: [`CallOptions::timeout`], or the channel's
/// [`timeout`](ChannelBuilder::timeout) (30 seconds by default). It is
/// sent to the server (`grpc-timeout`) and kept on this side too: a call
/// that outlives it ends with `DEADLINE_EXCEEDED` and is cancelled.
#[derive(Clone)]
pub struct Channel {
    inner: Rc<Inner>,
}

struct Inner {
    client: Client,
    /// `scheme://authority`, without a path.
    base: String,
    timeout: Duration,
    max_message: usize,
    compress: bool,
}

/// Settings for a [`Channel`]; [`build`](Self::build) makes it.
#[must_use = "a channel builder does nothing until it is built"]
pub struct ChannelBuilder {
    endpoint: String,
    timeout: Duration,
    connect_timeout: Duration,
    max_message: usize,
    compress: bool,
    tls: Option<TlsConnector>,
}

impl ChannelBuilder {
    /// The deadline of a call that sets none of its own (30 seconds by
    /// default).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How long opening a connection may take (10 seconds by default).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// The largest message the channel reads or writes, in bytes (4 MiB by
    /// default, at most 4 GiB less one byte). A larger one ends its call
    /// with `RESOURCE_EXHAUSTED` (`GIONG-E4102`). A compressed message
    /// counts by its size once decompressed.
    pub fn max_message_size(mut self, bytes: usize) -> Self {
        self.max_message = bytes.min(u32::MAX as usize);
        self
    }

    /// Whether requests are compressed with gzip (`false` by default; one
    /// call can decide otherwise with [`CallOptions::compress`]). The
    /// server must support gzip: one that does not answers
    /// `UNIMPLEMENTED`. Responses are read compressed or plain either way.
    pub fn compress(mut self, on: bool) -> Self {
        self.compress = on;
        self
    }

    /// The TLS settings for an `https` address, instead of the machine's
    /// certificate authorities: a private authority, a client certificate.
    pub fn tls(mut self, connector: TlsConnector) -> Self {
        self.tls = Some(connector);
        self
    }

    /// Makes the channel. Nothing is connected until the first call.
    ///
    /// # Errors
    ///
    /// `GIONG-E4107`
    /// ([`Code::GrpcEndpoint`](giong::error::Code::GrpcEndpoint)) if the
    /// address is not `http://host[:port]` or `https://host[:port]`.
    pub fn build(self) -> Result<Channel, Error> {
        let bad = |what: &str| {
            Error::new(
                ErrorCode::GrpcEndpoint,
                format!(
                    "{:?} is not the address of a gRPC server: {what}",
                    self.endpoint
                ),
                "give the scheme, host and port only, as in http://127.0.0.1:50051 or \
                 https://api.example.com.",
            )
        };
        let (https, rest) = if let Some(rest) = self.endpoint.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = self.endpoint.strip_prefix("http://") {
            (false, rest)
        } else {
            return Err(bad("it does not start with http:// or https://"));
        };
        let authority = rest.strip_suffix('/').unwrap_or(rest);
        if authority.is_empty() {
            return Err(bad("it has no host"));
        }
        if authority.contains(['/', '?', '#', '@', ' ']) {
            return Err(bad("it has a path, a query or credentials after the host"));
        }
        let mut builder = Client::builder()
            .timeout(self.timeout)
            .connect_timeout(self.connect_timeout)
            .redirects(0)
            .decompress(false)
            .http2_prior_knowledge(!https)
            .user_agent(concat!("grpc-giong/", env!("CARGO_PKG_VERSION")));
        if let Some(tls) = self.tls {
            builder = builder.tls(tls);
        }
        let scheme = if https { "https" } else { "http" };
        Ok(Channel {
            inner: Rc::new(Inner {
                client: builder.build(),
                base: format!("{scheme}://{authority}"),
                timeout: self.timeout,
                max_message: self.max_message,
                compress: self.compress,
            }),
        })
    }
}

impl fmt::Debug for ChannelBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelBuilder")
            .field("endpoint", &self.endpoint)
            .field("timeout", &self.timeout)
            .field("max_message_size", &self.max_message)
            .field("compress", &self.compress)
            .finish()
    }
}

/// What one call sets for itself: its deadline, its metadata, and whether
/// its requests are compressed.
#[derive(Clone, Debug, Default)]
#[must_use = "call options do nothing until they are given to a call"]
pub struct CallOptions {
    timeout: Option<Duration>,
    metadata: Metadata,
    compress: Option<bool>,
}

impl CallOptions {
    /// The channel's settings and no metadata.
    pub fn new() -> Self {
        Self::default()
    }

    /// How long the call may take, instead of the channel's timeout. For a
    /// streaming call this covers the whole stream.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// The metadata sent with the call.
    pub fn metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = metadata;
        self
    }

    /// Whether the call's requests are compressed with gzip, instead of
    /// what the channel is set to.
    pub fn compress(mut self, on: bool) -> Self {
        self.compress = Some(on);
        self
    }
}

impl Channel {
    /// A channel to `endpoint` (`http://host:port` or `https://host:port`)
    /// with the default settings. Nothing is connected until the first
    /// call.
    ///
    /// # Errors
    ///
    /// As [`ChannelBuilder::build`].
    pub fn new(endpoint: &str) -> Result<Self, Error> {
        Self::builder(endpoint).build()
    }

    /// A builder for a channel to `endpoint`, to change its settings.
    pub fn builder(endpoint: &str) -> ChannelBuilder {
        ChannelBuilder {
            endpoint: endpoint.to_owned(),
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_message: DEFAULT_MAX_MESSAGE,
            compress: false,
            tls: None,
        }
    }

    /// The request of a call to `path`, without its body.
    fn request(&self, path: &str, options: &CallOptions) -> (RequestBuilder, Deadline, bool) {
        let inner = &self.inner;
        let limit = options.timeout.unwrap_or(inner.timeout);
        let gzip = options.compress.unwrap_or(inner.compress);
        let mut request = inner
            .client
            .post(&format!("{}{path}", inner.base))
            .timeout(limit)
            .header("content-type", "application/grpc")
            .header("grpc-timeout", codec::format_timeout(limit))
            .header("grpc-accept-encoding", "identity,gzip");
        if gzip {
            request = request.header("grpc-encoding", "gzip");
        }
        for (name, value) in options.metadata.wire() {
            // Printable ASCII or base64: text.
            request = request.header(
                name,
                std::str::from_utf8(value.as_bytes()).unwrap_or_default(),
            );
        }
        let deadline = Deadline {
            at: codec::deadline_after(limit),
            limit,
        };
        (request, deadline, gzip)
    }

    /// Makes a unary call to the method at `path` (`/package.Service/Method`):
    /// sends `message`, and returns the one response with the metadata
    /// around it. The generated clients call this; use them.
    ///
    /// Cancel safe: dropping the future cancels the call, and the server's
    /// handler is dropped.
    ///
    /// # Errors
    ///
    /// The status the server ended the call with, when it is not `OK`;
    /// `UNAVAILABLE` (`GIONG-E4106`) if the connection could not be opened
    /// or broke; `DEADLINE_EXCEEDED` (`GIONG-E4108`) once the call's
    /// timeout has passed; `RESOURCE_EXHAUSTED` (`GIONG-E4102`) for a
    /// message past the channel's limit; `INTERNAL` (`GIONG-E4103`) if the
    /// server did not answer as gRPC specifies.
    pub async fn unary<Req, Resp>(
        &self,
        path: &str,
        message: &impl MessageOf<Type = Req>,
        options: CallOptions,
    ) -> Result<Reply<Resp>, Status>
    where
        Req: MessageType,
        Resp: MessageType,
    {
        let mut responses = self
            .server_streaming::<Req, Resp>(path, message, options)
            .await?;
        let Some(first) = responses.message().await? else {
            return Err(codec::protocol(
                "the server ended a call that answers with one message without sending it, \
                 and with the status OK",
            ));
        };
        if responses.message().await?.is_some() {
            return Err(codec::protocol(
                "the server sent more than one message in a call that answers with one",
            ));
        }
        let metadata = std::mem::take(&mut responses.metadata);
        let trailers = std::mem::take(&mut responses.trailers);
        Ok(Reply::received(first.into_payload(), metadata, trailers))
    }

    /// Makes a server-streaming call to the method at `path`: sends
    /// `message`, and returns the stream of responses once the server has
    /// started to answer. The generated clients call this; use them.
    ///
    /// Cancel safe: dropping the future cancels the call.
    ///
    /// # Errors
    ///
    /// As [`unary`](Self::unary), for what happens before the first
    /// response; later failures come from [`ResponseStream::message`].
    pub async fn server_streaming<Req, Resp>(
        &self,
        path: &str,
        message: &impl MessageOf<Type = Req>,
        options: CallOptions,
    ) -> Result<ResponseStream<Resp>, Status>
    where
        Req: MessageType,
        Resp: MessageType,
    {
        let (request, deadline, gzip) = self.request(path, &options);
        let frame = codec::encode_frame(message, gzip, self.inner.max_message)?;
        let sending = request.body(frame).send_streaming();
        let response = deadline.bound(sending).await?.map_err(transport)?;
        let mut stream = ResponseStream::new(
            Source::Open(Box::new(response)),
            deadline,
            self.inner.max_message,
        );
        stream.open().await?;
        Ok(stream)
    }

    /// Starts a bidirectional call to the method at `path`: returns the
    /// sink for the requests and the stream of responses, which may be
    /// used from two tasks. Nothing fails here: a call that cannot be made
    /// fails at the first [`ResponseStream::message`]. The generated
    /// clients call this; use them.
    ///
    /// Dropping the response stream cancels the call; dropping the sink
    /// ends the requests.
    pub fn bidi<Req, Resp>(
        &self,
        path: &str,
        options: CallOptions,
    ) -> (RequestSink<Req>, ResponseStream<Resp>)
    where
        Req: MessageType,
        Resp: MessageType,
    {
        let (request, deadline, gzip) = self.request(path, &options);
        let (request, body) = request.streamed_body();
        // In a task of its own: the request must be on its way for the
        // sink to write, whether or not the responses are being read.
        let sending = giong::spawn(request.send_streaming());
        let mut stream =
            ResponseStream::new(Source::Sending(sending), deadline, self.inner.max_message);
        let sink = RequestSink {
            body: Some(body),
            gzip,
            announced: gzip,
            cancelled: Rc::new(Cell::new(false)),
            max_message: self.inner.max_message,
            deadline,
            marker: PhantomData,
        };
        stream.cancelled = sink.cancelled.clone();
        (sink, stream)
    }

    /// Starts a client-streaming call to the method at `path`: the
    /// requests are sent on the value returned, and its
    /// [`finish`](ClientStreaming::finish) gives the one response. The
    /// generated clients call this; use them.
    pub fn client_streaming<Req, Resp>(
        &self,
        path: &str,
        options: CallOptions,
    ) -> ClientStreaming<Req, Resp>
    where
        Req: MessageType,
        Resp: MessageType,
    {
        let (requests, responses) = self.bidi(path, options);
        ClientStreaming {
            requests,
            responses,
        }
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("endpoint", &self.inner.base)
            .field("timeout", &self.inner.timeout)
            .field("max_message_size", &self.inner.max_message)
            .field("compress", &self.inner.compress)
            .finish()
    }
}

/// When a call must have ended.
#[derive(Clone, Copy)]
struct Deadline {
    at: Instant,
    limit: Duration,
}

impl Deadline {
    /// Runs `work` until the deadline.
    async fn bound<T>(&self, work: impl std::future::Future<Output = T>) -> Result<T, Status> {
        let left = self.at.saturating_duration_since(Instant::now());
        timeout(left, work).await.map_err(|_| self.passed())
    }

    fn passed(&self) -> Status {
        Status::raised(
            Code::DeadlineExceeded,
            ErrorCode::GrpcDeadline,
            format!(
                "the call did not end within its deadline of {:?}",
                self.limit
            ),
            "give the call more time (CallOptions::timeout, or ChannelBuilder::timeout \
             for every call), or find out why the server is slow.",
        )
    }
}

/// An HTTP failure as the status of the call it carried.
fn transport(error: giong_http::client::Error) -> Status {
    match error.kind() {
        ErrorKind::Timeout => Status::raised(
            Code::DeadlineExceeded,
            ErrorCode::GrpcDeadline,
            format!("the call did not end within its deadline: {error}"),
            "give the call more time (CallOptions::timeout, or ChannelBuilder::timeout \
             for every call), or find out why the server is slow.",
        ),
        ErrorKind::Connect | ErrorKind::Closed | ErrorKind::Proxy => Status::raised(
            Code::Unavailable,
            ErrorCode::GrpcTransport,
            format!("the connection to the server failed: {error}"),
            "check that the server is running and reachable at the channel's address; \
             a call that failed this way may be tried again if it is safe to repeat.",
        ),
        _ => codec::protocol(format!("the HTTP exchange under the call failed: {error}")),
    }
}

fn ended() -> Status {
    Status::raised(
        Code::Cancelled,
        ErrorCode::GrpcCancelled,
        "the call has ended, so nothing more can be sent on it",
        "read the status of the call from its responses (ResponseStream::message, or \
         ClientStreaming::finish): the server ended it, or the response stream was \
         dropped.",
    )
}

/// The requests of a client-streaming or bidirectional call.
///
/// Messages go out in order, one in flight: [`send`](Self::send) waits
/// until the connection has taken the previous one, so a slow server slows
/// the sender down. Dropping the sink (or [`finish`](Self::finish)) tells
/// the server that the requests are complete.
pub struct RequestSink<M> {
    body: Option<RequestBody>,
    gzip: bool,
    /// The call started with `grpc-encoding: gzip`.
    announced: bool,
    /// Set by `cancel`, for the response side to say so.
    cancelled: Rc<Cell<bool>>,
    max_message: usize,
    /// The call's: a send that waits for room waits no longer.
    deadline: Deadline,
    marker: PhantomData<fn(M)>,
}

impl<M: MessageType> RequestSink<M> {
    /// Sends one message.
    ///
    /// Cancel safe: if the future is dropped before it returns, the
    /// message was not sent.
    ///
    /// # Errors
    ///
    /// `CANCELLED` (`GIONG-E4109`) when the call has ended: the server
    /// ended it (its status is on the response side), the connection
    /// broke, or the response stream was dropped. `RESOURCE_EXHAUSTED`
    /// (`GIONG-E4102`) for a message past the channel's limit; the call
    /// can go on with smaller ones. `DEADLINE_EXCEEDED` (`GIONG-E4108`)
    /// when the call's deadline passes while the message waits for room,
    /// because the server does not read.
    pub async fn send(&mut self, message: &impl MessageOf<Type = M>) -> Result<(), Status> {
        let frame = codec::encode_frame(message, self.gzip, self.max_message)?;
        let Some(body) = &self.body else {
            return Err(ended());
        };
        let mut send = pin!(body.send(frame));
        // Most sends find room at once: no timer for those.
        let first = poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx))).await;
        let sent = match first {
            Poll::Ready(sent) => sent,
            Poll::Pending => self.deadline.bound(send).await?,
        };
        sent.map_err(|_| {
            // The call's deadline ends the request too.
            if Instant::now() >= self.deadline.at {
                self.deadline.passed()
            } else {
                ended()
            }
        })
    }

    /// Whether the messages sent from now on are compressed, on a call
    /// that started with compression ([`CallOptions::compress`] or the
    /// channel's default): it can change from one message to the next.
    ///
    /// # Errors
    ///
    /// `FAILED_PRECONDITION` when turning it on for a call that started
    /// without compression: the request's headers, which name the
    /// compression, are gone by then. Start the call with
    /// `CallOptions::compress(true)`, then turn it off and on as needed.
    pub fn compress(&mut self, on: bool) -> Result<(), Status> {
        if on && !self.announced {
            return Err(Status::new(
                Code::FailedPrecondition,
                "the call started without compression, so its messages cannot be compressed \
                 now; start it with CallOptions::compress(true)",
            ));
        }
        self.gzip = on;
        Ok(())
    }

    /// Ends the requests: the server sees the end of the stream. The same
    /// as dropping the sink.
    pub fn finish(mut self) {
        self.body = None;
    }

    /// Cancels the call: the stream is reset, and the server's handler is
    /// dropped. The response side then ends with `CANCELLED`
    /// (`GIONG-E4109`), unless the server's status had already arrived.
    pub fn cancel(mut self) {
        if let Some(body) = self.body.take() {
            self.cancelled.set(true);
            body.abort();
        }
    }

    /// `true` once nothing more can be sent (see [`send`](Self::send)).
    pub fn is_closed(&self) -> bool {
        self.body.as_ref().map_or(true, RequestBody::is_closed)
    }
}

impl<M> fmt::Debug for RequestSink<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestSink")
            .field("open", &self.body.is_some())
            .finish()
    }
}

/// Where the responses of a call come from.
enum Source {
    /// The request is being sent, in a task; the response has not started.
    Sending(JoinHandle<Result<StreamingResponse, giong_http::client::Error>>),
    Open(Box<StreamingResponse>),
    /// The call is over.
    Done,
}

/// The responses of a server-streaming or bidirectional call, read one at
/// a time.
///
/// Dropping it before the end cancels the call: the stream is reset, and
/// the server's handler is dropped.
pub struct ResponseStream<M> {
    source: Source,
    deadline: Deadline,
    deframer: Deframer,
    max_message: usize,
    metadata: Metadata,
    trailers: Metadata,
    /// The status from a response without a body (trailers-only).
    early: Option<Status>,
    opened: bool,
    /// The request side cancelled the call (`RequestSink::cancel`).
    cancelled: Rc<Cell<bool>>,
    marker: PhantomData<fn() -> M>,
}

impl<M: MessageType> ResponseStream<M> {
    fn new(source: Source, deadline: Deadline, max_message: usize) -> Self {
        Self {
            source,
            deadline,
            deframer: Deframer::new(max_message, false),
            max_message,
            metadata: Metadata::new(),
            trailers: Metadata::new(),
            early: None,
            opened: false,
            cancelled: Rc::new(Cell::new(false)),
            marker: PhantomData,
        }
    }

    fn fail(&mut self, status: Status) -> Status {
        // Dropping the response resets the stream.
        self.abort();
        if self.cancelled.get() {
            // Whatever the reset stream failed with, this is why.
            return Status::raised(
                Code::Cancelled,
                ErrorCode::GrpcCancelled,
                "the call was cancelled on this side (RequestSink::cancel or \
                 ClientStreaming::cancel)",
                "nothing to fix if the cancel was meant; the server may have done part \
                 of the work.",
            );
        }
        // A server that enforces the deadline itself resets the stream when
        // it passes, which can get here before this side's own timer does.
        if status.error_code() == Some(ErrorCode::GrpcTransport)
            && Instant::now() >= self.deadline.at
        {
            return self.deadline.passed();
        }
        status
    }

    fn abort(&mut self) {
        if let Source::Sending(task) = &self.source {
            task.abort();
        }
        self.source = Source::Done;
    }

    /// Waits for the head of the response and reads it.
    async fn open(&mut self) -> Result<(), Status> {
        if self.opened {
            return Ok(());
        }
        if let Source::Sending(task) = &mut self.source {
            let joined = match self.deadline.bound(task).await {
                Ok(joined) => joined,
                Err(status) => return Err(self.fail(status)),
            };
            let response = match joined {
                Ok(Ok(response)) => response,
                Ok(Err(e)) => return Err(self.fail(transport(e))),
                Err(_) => {
                    return Err(self.fail(codec::protocol(
                        "the task sending the request stopped before the response started",
                    )))
                }
            };
            self.source = Source::Open(Box::new(response));
        }
        let Source::Open(response) = &self.source else {
            return Err(ended());
        };
        self.opened = true;
        let status = response.status();
        let content_type = response.header("content-type").unwrap_or_default();
        if status != 200 || !codec::is_grpc_content_type(content_type.as_bytes()) {
            let code = if status == 200 {
                Code::Unknown
            } else {
                Code::from_http(status)
            };
            let failure = Status::raised(
                code,
                ErrorCode::GrpcProtocol,
                format!(
                    "the server answered with HTTP status {status} and content type \
                     {content_type:?} instead of a gRPC response"
                ),
                "check that the channel's address is a gRPC server and not a proxy's \
                 error page or another HTTP service.",
            );
            return Err(self.fail(failure));
        }
        if response.version() != Version::Http2 {
            let failure = codec::protocol(
                "the server answered over HTTP/1.1; gRPC needs HTTP/2, which the server \
                 must offer through ALPN",
            );
            return Err(self.fail(failure));
        }
        let gzip = match response.header("grpc-encoding") {
            None | Some("identity") => false,
            Some("gzip") => true,
            Some(other) => {
                let failure = Status::raised(
                    Code::Internal,
                    ErrorCode::GrpcEncoding,
                    format!(
                        "the server compresses its messages with {other:?}, which this \
                         client did not offer"
                    ),
                    "the server must choose from grpc-accept-encoding: identity or gzip.",
                );
                return Err(self.fail(failure));
            }
        };
        self.deframer = Deframer::new(self.max_message, gzip);
        let mut metadata = Metadata::new();
        let mut bad = None;
        for (name, value) in response.headers() {
            if let Err(name) = metadata.received(&name.to_ascii_lowercase(), value) {
                bad = Some(name);
            }
        }
        if let Some(name) = bad {
            let failure = codec::protocol(format!(
                "the value of the metadata name {name} is not base64"
            ));
            return Err(self.fail(failure));
        }
        // A response without messages carries its status in the headers.
        if let Some(status) = response.header("grpc-status") {
            self.early = Some(read_status(
                status.as_bytes(),
                response
                    .headers()
                    .find(|(n, _)| n.eq_ignore_ascii_case("grpc-message"))
                    .map(|(_, v)| v),
                response
                    .headers()
                    .find(|(n, _)| n.eq_ignore_ascii_case("grpc-status-details-bin"))
                    .map(|(_, v)| v),
            ));
        }
        self.metadata = metadata;
        Ok(())
    }

    /// The metadata the server sent before its first message, once the
    /// response has started: waits for it if it has not.
    ///
    /// Cancel safe: a dropped future loses nothing, and the next call
    /// waits again.
    ///
    /// # Errors
    ///
    /// As [`message`](Self::message), if the call fails before the
    /// response starts.
    pub async fn metadata(&mut self) -> Result<&Metadata, Status> {
        self.open().await?;
        Ok(&self.metadata)
    }

    /// The next message, or `None` once the server has ended the call with
    /// the status `OK`.
    ///
    /// Not cancel safe: a message whose bytes are arriving when the future
    /// is dropped is lost, so drop the stream then.
    ///
    /// # Errors
    ///
    /// The status the server ended the call with, when it is not `OK`;
    /// `UNAVAILABLE` (`GIONG-E4106`) if the connection could not be opened
    /// or broke; `DEADLINE_EXCEEDED` (`GIONG-E4108`) once the call's
    /// timeout has passed; `RESOURCE_EXHAUSTED` (`GIONG-E4102`) for a
    /// message past the channel's limit; `INTERNAL` (`GIONG-E4103`) if the
    /// server did not answer as gRPC specifies. After an error, or `None`,
    /// the call is over and every further call returns `None`.
    pub async fn message(&mut self) -> Result<Option<Received<M>>, Status> {
        // A message already received is returned without the future that
        // waits for more: that one is large (it holds the wait for the
        // response and the deadline), and setting it up for each message
        // cost more than taking the message out.
        if self.opened && matches!(self.source, Source::Open(_)) {
            match self.deframer.next() {
                Ok(Some(payload)) => return Ok(Some(Received::new(payload))),
                Ok(None) => {}
                Err(status) => return Err(self.fail(status)),
            }
        }
        Box::pin(self.wait_message()).await
    }

    /// [`message`](Self::message) when no message is ready.
    async fn wait_message(&mut self) -> Result<Option<Received<M>>, Status> {
        if matches!(self.source, Source::Done) {
            return Ok(None);
        }
        self.open().await?;
        loop {
            match self.deframer.next() {
                Ok(Some(payload)) => return Ok(Some(Received::new(payload))),
                Ok(None) => {}
                Err(status) => return Err(self.fail(status)),
            }
            let Source::Open(response) = &mut self.source else {
                return Ok(None);
            };
            let piece = match self.deadline.bound(response.chunk()).await {
                Ok(Ok(piece)) => piece,
                Ok(Err(e)) => return Err(self.fail(transport(e))),
                Err(status) => return Err(self.fail(status)),
            };
            match piece {
                Some(piece) => self.deframer.push(piece),
                None => return self.end(),
            }
        }
    }

    /// The body ended: the status is in the trailers.
    fn end(&mut self) -> Result<Option<Received<M>>, Status> {
        let Source::Open(response) = std::mem::replace(&mut self.source, Source::Done) else {
            return Ok(None);
        };
        self.deframer.finish()?;
        let mut trailers = Metadata::new();
        let status = match response.trailers() {
            Some(fields) => {
                for (name, value) in fields.iter() {
                    trailers.received(name, value).map_err(|name| {
                        codec::protocol(format!(
                            "the value of the metadata name {name} is not base64"
                        ))
                    })?;
                }
                match fields.get("grpc-status") {
                    Some(status) => Some(read_status(
                        status,
                        fields.get("grpc-message"),
                        fields.get("grpc-status-details-bin"),
                    )),
                    None => self.early.take(),
                }
            }
            None => {
                // Trailers-only: the headers were the trailers.
                trailers = self.metadata.clone();
                self.early.take()
            }
        };
        let Some(mut status) = status else {
            return Err(codec::protocol(
                "the server ended the response without a grpc-status",
            ));
        };
        if status.code() == Code::Ok {
            self.trailers = trailers;
            return Ok(None);
        }
        *status.metadata_mut() = trailers;
        Err(status)
    }

    /// The metadata the server sent after its last message, once
    /// [`message`](Self::message) has returned `None`; empty before. For a
    /// call that failed, the metadata is in the status
    /// ([`Status::metadata`]).
    pub fn trailers(&self) -> &Metadata {
        &self.trailers
    }
}

impl<M> Drop for ResponseStream<M> {
    fn drop(&mut self) {
        if let Source::Sending(task) = &self.source {
            task.abort();
        }
    }
}

impl<M> fmt::Debug for ResponseStream<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = match self.source {
            Source::Sending(_) => "sending",
            Source::Open(_) => "open",
            Source::Done => "done",
        };
        f.debug_struct("ResponseStream")
            .field("state", &state)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// A status from `grpc-status`, `grpc-message` and
/// `grpc-status-details-bin`.
fn read_status(code: &[u8], message: Option<&[u8]>, details: Option<&[u8]>) -> Status {
    let number = std::str::from_utf8(code)
        .ok()
        .and_then(|s| s.parse::<u32>().ok());
    let Some(number) = number else {
        return codec::protocol(format!(
            "the grpc-status {:?} is not a number",
            String::from_utf8_lossy(code)
        ));
    };
    let mut status = Status::new(
        Code::from_number(number),
        message.map(codec::decode_message).unwrap_or_default(),
    );
    if let Some(details) = details.and_then(crate::metadata::base64_decode) {
        status = status.with_details(details);
    }
    status
}

/// A client-streaming call in progress: send the requests, then
/// [`finish`](Self::finish) for the response.
pub struct ClientStreaming<Req, Resp> {
    requests: RequestSink<Req>,
    responses: ResponseStream<Resp>,
}

impl<Req: MessageType, Resp: MessageType> ClientStreaming<Req, Resp> {
    /// Sends one request message (see [`RequestSink::send`]).
    ///
    /// Cancel safe: if the future is dropped before it returns, the
    /// message was not sent.
    ///
    /// # Errors
    ///
    /// As [`RequestSink::send`]. When the call has ended,
    /// [`finish`](Self::finish) says how.
    pub async fn send(&mut self, message: &impl MessageOf<Type = Req>) -> Result<(), Status> {
        self.requests.send(message).await
    }

    /// Whether the messages sent from now on are compressed (see
    /// [`RequestSink::compress`]).
    ///
    /// # Errors
    ///
    /// As [`RequestSink::compress`].
    pub fn compress(&mut self, on: bool) -> Result<(), Status> {
        self.requests.compress(on)
    }

    /// Ends the requests and waits for the server's one response.
    ///
    /// Cancel safe: dropping the future cancels the call.
    ///
    /// # Errors
    ///
    /// As [`Channel::unary`].
    pub async fn finish(self) -> Result<Reply<Resp>, Status> {
        let Self {
            requests,
            mut responses,
        } = self;
        requests.finish();
        let Some(first) = responses.message().await? else {
            return Err(codec::protocol(
                "the server ended a call that answers with one message without sending it, \
                 and with the status OK",
            ));
        };
        if responses.message().await?.is_some() {
            return Err(codec::protocol(
                "the server sent more than one message in a call that answers with one",
            ));
        }
        let metadata = std::mem::take(&mut responses.metadata);
        let trailers = std::mem::take(&mut responses.trailers);
        Ok(Reply::received(first.into_payload(), metadata, trailers))
    }

    /// Cancels the call (see [`RequestSink::cancel`]).
    pub fn cancel(self) {
        self.requests.cancel();
    }
}

impl<Req, Resp> fmt::Debug for ClientStreaming<Req, Resp> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientStreaming")
            .field("requests", &self.requests)
            .field("responses", &self.responses)
            .finish()
    }
}
