//! One connection: opening it, sending a request on it, and reading the
//! response.

use std::future::Future;
use std::io;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Wake, Waker};
use std::time::Instant;

use giong::net::TcpStream;
use giong::RecvBuf;
use giong_tls::{TlsConnector, TlsStream};

use super::error::{Error, ErrorKind};
use super::head::{parse_head, Framing, Head};
use super::proxy::{Route, Server};
use super::ClientBuilder;
use crate::chunked::{Decoder, Progress};
use crate::parse::find_head_end;

/// The socket under a connection.
pub(crate) enum Stream {
    Tcp(TcpStream),
    Tls(Box<TlsStream>),
}

/// Received bytes, from either socket.
pub(crate) enum Chunk {
    Pooled(RecvBuf),
    Owned(Vec<u8>),
}

impl Deref for Chunk {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Chunk::Pooled(b) => b,
            Chunk::Owned(v) => v,
        }
    }
}

impl Stream {
    /// The next bytes; empty at the end of the stream.
    pub(crate) async fn recv(&self) -> io::Result<Chunk> {
        match self {
            Stream::Tcp(s) => s.recv().await.map(Chunk::Pooled),
            Stream::Tls(s) => s.recv().await.map(Chunk::Owned),
        }
    }

    pub(crate) async fn write_all(&self, buf: Vec<u8>) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.write_all(buf).await.0,
            Stream::Tls(s) => s.write_all(buf).await.0,
        }
    }

    /// Ends our side of the connection: `close_notify` over TLS.
    pub(crate) async fn close(&self) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.shutdown(std::net::Shutdown::Write),
            Stream::Tls(s) => s.shutdown().await,
        }
    }

    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Tcp(s) => s,
            Stream::Tls(s) => s.tcp(),
        }
    }

    /// Whether an end of stream just received can be trusted to be the
    /// server's: over TLS only a `close_notify` alert is authenticated, and
    /// a TCP close without one may come from anyone on the path.
    fn ended_cleanly(&self) -> bool {
        match self {
            Stream::Tcp(_) => true,
            Stream::Tls(s) => s.received_close_notify(),
        }
    }
}

/// An open connection to one origin.
pub(crate) struct Conn {
    stream: Stream,
    /// When it last finished a response.
    pub(crate) idle_since: Instant,
}

/// A response whose head has been read. Its body is read with
/// [`Body::read`], on the same connection.
pub(crate) struct Answer {
    pub(crate) head: Head,
    /// Also holds the head's bytes, which `head`'s ranges point into.
    pub(crate) body: Body,
}

/// The body of a response, read as it arrives: the framing (length,
/// chunked, until the connection closes) is taken off, nothing else.
pub(crate) struct Body {
    kind: Kind,
    /// The head's bytes (the first `head` bytes), then bytes received;
    /// those from `pos` on are not given out yet. It is the buffer the head
    /// was read into, so neither the head nor what came with it is copied.
    buf: Vec<u8>,
    head: usize,
    pos: usize,
    /// The connection can carry another request, as far as known.
    reusable: bool,
    done: bool,
}

enum Kind {
    Empty,
    Length(u64),
    Chunked(Decoder),
    UntilClose,
}

/// Why an exchange failed.
pub(crate) enum Failure {
    /// No byte of a response arrived: on a kept-alive connection, the server
    /// may have closed it before it read the request.
    NothingReceived(Error),
    Other(Error),
}

impl Failure {
    pub(crate) fn into_error(self) -> Error {
        match self {
            Failure::NothingReceived(e) | Failure::Other(e) => e,
        }
    }
}

impl Conn {
    /// Opens a connection on `route`: to the origin, or to the proxy (with
    /// a tunnel through it for `https`), with TLS when the origin is
    /// `https`.
    pub(crate) async fn open(
        route: &Route,
        tls: Option<&TlsConnector>,
        config: &ClientBuilder,
    ) -> Result<Conn, Error> {
        let origin = &route.origin;
        let connect_timeout = config.connect_timeout;
        let connect = async {
            let tcp = match &route.proxy {
                None => TcpStream::connect((origin.host.as_str(), origin.port))
                    .await
                    .map_err(|e| {
                        Error::io(
                            ErrorKind::Connect,
                            format!(
                                "cannot connect to {origin} (is the host name right, and the server up?)"
                            ),
                            e,
                        )
                    })?,
                Some(proxy) => TcpStream::connect((proxy.host.as_str(), proxy.port))
                    .await
                    .map_err(|e| {
                        Error::io(
                            ErrorKind::Connect,
                            format!(
                                "cannot connect to the proxy {proxy} (for {origin}); check the \
                                 proxy's address, or leave {} out of it with Proxy::no_proxy",
                                origin.host
                            ),
                            e,
                        )
                    })?,
            };
            let _ = tcp.set_nodelay(true);
            if let (Some(proxy), true) = (&route.proxy, origin.https) {
                tunnel(&tcp, proxy, route, config.max_head_size).await?;
            }
            let stream = match tls {
                None => Stream::Tcp(tcp),
                Some(connector) => {
                    let tls = connector.connect(&origin.host, tcp).await.map_err(|e| {
                        Error::io(
                            ErrorKind::Connect,
                            format!("the TLS handshake with {origin} failed"),
                            e,
                        )
                    })?;
                    Stream::Tls(Box::new(tls))
                }
            };
            Ok(Conn {
                stream,
                idle_since: Instant::now(),
            })
        };
        match giong::time::timeout(connect_timeout, connect).await {
            Ok(result) => result,
            Err(_) => {
                let through = match &route.proxy {
                    Some(proxy) => format!(" through the proxy {proxy}"),
                    None => String::new(),
                };
                Err(Error::new(
                    ErrorKind::Connect,
                    format!(
                        "connecting to {origin}{through} took longer than the connect timeout of \
                         {connect_timeout:?}; check that the host is reachable from here (a \
                         firewall that drops packets looks like this), or raise \
                         ClientBuilder::connect_timeout"
                    ),
                ))
            }
        }
    }

    /// Whether TLS chose HTTP/2 through ALPN.
    pub(crate) fn is_h2(&self) -> bool {
        match &self.stream {
            Stream::Tcp(_) => false,
            Stream::Tls(s) => s.alpn_protocol().as_deref() == Some(b"h2"),
        }
    }

    /// The socket, to speak HTTP/2 on.
    pub(crate) fn into_stream(self) -> Stream {
        self.stream
    }

    /// Whether a connection that sat idle can no longer carry a request: the
    /// server closed it, it failed, or the server sent something nobody
    /// asked for. Such bytes (a `408 Request Timeout` sent just before the
    /// server closes, the rest of a body longer than its `Content-Length`)
    /// would otherwise be read as the response to the next request.
    pub(crate) fn is_spoiled(&self) -> bool {
        // What has been received already. Over TLS this decrypts it: session
        // tickets are taken in and produce no data.
        let mut recv = pin!(self.stream.recv());
        let waker = Waker::from(Arc::new(Noop));
        if recv
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready()
        {
            // Data, the end of the stream, or an error.
            return true;
        }
        let mut events = libc::POLLRDHUP;
        if let Stream::Tcp(_) = self.stream {
            // Bytes the socket holds that have not been received yet. Over
            // TLS they may be session tickets, which are no response.
            events |= libc::POLLIN;
        }
        let mut fd = libc::pollfd {
            fd: self.stream.tcp().as_raw_fd(),
            events,
            revents: 0,
        };
        // SAFETY: `fd` is one valid `pollfd` for a descriptor this connection
        // owns, and a timeout of 0 makes `poll` return at once.
        let n = unsafe { libc::poll(&mut fd, 1, 0) };
        n != 0
    }

    /// Sends `request` and reads the head of the response to it, after
    /// any 1xx (informational) responses.
    pub(crate) async fn send(
        &mut self,
        request: Vec<u8>,
        head_request: bool,
        max_head_size: usize,
        what: &str,
    ) -> Result<Answer, Failure> {
        if let Err(e) = self.stream.write_all(request).await {
            return Err(Failure::NothingReceived(Error::io(
                ErrorKind::Closed,
                format!("{what}: sending the request failed"),
                e,
            )));
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut received_any = false;
        let mut scanned: usize = 0;
        let mut informational = 0;

        let head = loop {
            let found = find_head_end(&buf, scanned.saturating_sub(3));
            if found.unwrap_or(buf.len()) > max_head_size {
                return Err(Failure::Other(Error::new(
                    ErrorKind::TooLarge,
                    format!(
                        "{what}: the response head is longer than {max_head_size} bytes (ClientBuilder::max_head_size)"
                    ),
                )));
            }
            if let Some(end) = found {
                let head = parse_head(&buf[..end]).map_err(|m| protocol(what, &m))?;
                if head.status == 101 {
                    return Err(protocol(
                        what,
                        "the server switched protocols (101), which this client does not support",
                    ));
                }
                if head.status < 200 {
                    informational += 1;
                    if informational > 32 {
                        return Err(protocol(what, "the server sent more than 32 1xx responses"));
                    }
                    buf.drain(..end);
                    scanned = 0;
                    continue;
                }
                break head;
            }
            // A bare line feed would never end the head: say so now rather
            // than wait for the server to close.
            if (scanned..buf.len()).any(|at| buf[at] == b'\n' && (at == 0 || buf[at - 1] != b'\r'))
            {
                return Err(protocol(
                    what,
                    "a line of the head ends in a bare line feed",
                ));
            }
            scanned = buf.len();
            let chunk = self.recv(what, received_any, "the response head").await?;
            received_any = true;
            buf.extend_from_slice(&chunk);
        };

        let pos = head.len;
        let kind = match head.framing(head_request) {
            Framing::None => Kind::Empty,
            Framing::Length(n) => Kind::Length(n),
            Framing::Chunked => Kind::Chunked(Decoder::new(usize::MAX)),
            Framing::UntilClose => Kind::UntilClose,
        };
        let body = Body {
            kind,
            buf,
            head: pos,
            pos,
            reusable: !head.close,
            done: false,
        };
        Ok(Answer { head, body })
    }

    /// The next bytes of the response; the end of the stream is an error.
    async fn recv(&self, what: &str, received_any: bool, part: &str) -> Result<Chunk, Failure> {
        let fail = |e: Error| {
            if received_any {
                Failure::Other(e)
            } else {
                Failure::NothingReceived(e)
            }
        };
        match self.stream.recv().await {
            Ok(chunk) if !chunk.is_empty() => Ok(chunk),
            Ok(_) if !received_any => Err(fail(Error::new(
                ErrorKind::Closed,
                format!("{what}: the server closed the connection without answering"),
            ))),
            Ok(_) => Err(fail(Error::new(
                ErrorKind::Closed,
                format!("{what}: the server closed the connection in the middle of {part}"),
            ))),
            Err(e) => Err(fail(Error::io(
                ErrorKind::Closed,
                format!("{what}: reading {part} failed"),
                e,
            ))),
        }
    }
}

/// Wakes nobody: for looking at what a stream holds without waiting.
struct Noop;

impl Wake for Noop {
    fn wake(self: Arc<Self>) {}
}

fn protocol(what: &str, message: &str) -> Failure {
    Failure::Other(Error::new(
        ErrorKind::Protocol,
        format!("{what}: the response is not valid HTTP/1.1: {message}"),
    ))
}

/// Asks `proxy` for a tunnel to the route's origin (`CONNECT host:port`)
/// and reads its answer. The bytes that follow a 2xx answer belong to the
/// server, so the answer is read without reading past it: the server only
/// speaks once TLS has, and anything after the answer's head is an error.
async fn tunnel(
    tcp: &TcpStream,
    proxy: &Server,
    route: &Route,
    max_head_size: usize,
) -> Result<(), Error> {
    let origin = &route.origin;
    let authority = if origin.host.contains(':') {
        format!("[{}]:{}", origin.host, origin.port)
    } else {
        format!("{}:{}", origin.host, origin.port)
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let Some(auth) = &proxy.authorization {
        request.push_str("Proxy-Authorization: ");
        request.push_str(auth);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    let what = format!("the proxy {proxy}, asked for a tunnel to {authority},");
    tcp.write_all(request.into_bytes()).await.0.map_err(|e| {
        Error::io(
            ErrorKind::Connect,
            format!("{what} could not be sent the request"),
            e,
        )
    })?;
    let mut buf = Vec::new();
    let end = loop {
        if let Some(end) = find_head_end(&buf, 0) {
            break end;
        }
        if buf.len() > max_head_size {
            return Err(Error::proxy(
                None,
                format!("{what} answered with a head longer than {max_head_size} bytes"),
            ));
        }
        let chunk = tcp.recv().await.map_err(|e| {
            Error::io(
                ErrorKind::Connect,
                format!("{what} failed while answering"),
                e,
            )
        })?;
        if chunk.is_empty() {
            return Err(Error::new(
                ErrorKind::Connect,
                format!("{what} closed the connection without answering"),
            ));
        }
        buf.extend_from_slice(&chunk);
    };
    let head = parse_head(&buf[..end])
        .map_err(|m| Error::proxy(None, format!("{what} sent an answer that is not HTTP: {m}")))?;
    match head.status {
        200..=299 if buf.len() == end => Ok(()),
        200..=299 => Err(Error::proxy(
            Some(head.status),
            format!("{what} sent bytes after its answer, before TLS began"),
        )),
        407 => Err(Error::proxy(
            Some(407),
            format!(
                "{what} asks for credentials (407 Proxy Authentication Required); {}",
                if proxy.authorization.is_some() {
                    "the user name and password in the proxy URL were refused"
                } else {
                    "put them in the proxy URL: http://user:password@host:port"
                }
            ),
        )),
        status => Err(Error::proxy(
            Some(status),
            format!(
                "{what} refused it with status {status}{}",
                match status {
                    403 =>
                        "; the proxy's rules do not allow this host or port (add it to \
                            Proxy::no_proxy if it should be reached directly)",
                    502 | 504 => "; the proxy could not reach the server",
                    _ => "",
                }
            ),
        )),
    }
}

impl Body {
    /// Whether the whole body has been read.
    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// Whether the connection can carry another request: the body was
    /// read to its end, the server did not ask to close, and nothing came
    /// after the body.
    pub(crate) fn reusable(&self) -> bool {
        self.done && self.reusable
    }

    /// The head's bytes, which the head's ranges point into. Empty once
    /// [`Body::take_with_head`] took them.
    pub(crate) fn head_bytes(&self) -> &[u8] {
        &self.buf[..self.head]
    }

    /// The length the server announced, if it did.
    pub(crate) fn length(&self) -> Option<u64> {
        match self.kind {
            Kind::Length(n) if !self.done => Some(n),
            Kind::Empty => Some(0),
            _ => None,
        }
    }

    /// For a body with a length, read whole: the buffer the head was read
    /// into, head and all, with as much of the body as came with it, to
    /// append the rest to. Saves copying what came with the head.
    pub(crate) fn take_with_head(&mut self) -> Option<Vec<u8>> {
        let Kind::Length(left) = &mut self.kind else {
            return None;
        };
        if self.done {
            return None;
        }
        let n = (*left).min((self.buf.len() - self.pos) as u64) as usize;
        let mut data = std::mem::take(&mut self.buf);
        if data.len() > self.pos + n {
            // Bytes after the body belong to no request.
            self.reusable = false;
            data.truncate(self.pos + n);
        }
        *left -= n as u64;
        self.head = 0;
        self.pos = 0;
        Some(data)
    }

    /// Appends the next bytes of the body to `out`, waiting for them if
    /// none have arrived. Returns `false`, and appends nothing, once the
    /// body has ended.
    pub(crate) async fn read(
        &mut self,
        conn: &Conn,
        out: &mut Vec<u8>,
        what: &str,
    ) -> Result<bool, Error> {
        loop {
            if self.done {
                return Ok(false);
            }
            match &mut self.kind {
                Kind::Empty | Kind::Length(0) => {
                    // Bytes after the body belong to no request.
                    self.reusable &= self.pos == self.buf.len();
                    self.buf.truncate(self.head);
                    self.pos = self.head;
                    self.done = true;
                    return Ok(false);
                }
                Kind::Length(left) => {
                    if self.pos == self.buf.len() {
                        let chunk = conn
                            .recv(what, true, "the response body")
                            .await
                            .map_err(Failure::into_error)?;
                        let n = (*left).min(chunk.len() as u64) as usize;
                        out.extend_from_slice(&chunk[..n]);
                        *left -= n as u64;
                        self.buf.truncate(self.head);
                        self.buf.extend_from_slice(&chunk[n..]);
                        self.pos = self.head;
                    } else {
                        let n = (*left).min((self.buf.len() - self.pos) as u64) as usize;
                        out.extend_from_slice(&self.buf[self.pos..self.pos + n]);
                        self.pos += n;
                        *left -= n as u64;
                    }
                    return Ok(true);
                }
                Kind::Chunked(decoder) => match decoder.feed(&self.buf[self.pos..]) {
                    Ok(Progress::Done(n)) => {
                        self.reusable &= self.pos + n == self.buf.len();
                        self.buf.truncate(self.head);
                        self.pos = self.head;
                        self.done = true;
                        let any = !decoder.body.is_empty();
                        out.append(&mut decoder.body);
                        return Ok(any);
                    }
                    Ok(Progress::NeedMore) => {
                        self.pos += decoder.consume();
                        if !decoder.body.is_empty() {
                            out.append(&mut decoder.body);
                            return Ok(true);
                        }
                        self.buf.drain(self.head..self.pos);
                        self.pos = self.head;
                        let chunk = conn
                            .recv(what, true, "the chunked response body")
                            .await
                            .map_err(Failure::into_error)?;
                        self.buf.extend_from_slice(&chunk);
                    }
                    Err(_) => {
                        return Err(protocol(what, "the chunked body is malformed").into_error());
                    }
                },
                Kind::UntilClose => {
                    self.reusable = false;
                    if self.pos < self.buf.len() {
                        out.extend_from_slice(&self.buf[self.pos..]);
                        self.buf.truncate(self.head);
                        self.pos = self.head;
                        return Ok(true);
                    }
                    let chunk = conn.stream.recv().await.map_err(|e| {
                        Error::io(
                            ErrorKind::Closed,
                            format!("{what}: reading the response body failed"),
                            e,
                        )
                    })?;
                    if chunk.is_empty() {
                        // Without close_notify the body may have been cut
                        // short on the way (a truncation attack).
                        if !conn.stream.ended_cleanly() {
                            return Err(Error::new(
                                ErrorKind::Closed,
                                format!(
                                    "{what}: the server closed the connection without a TLS \
                                     close_notify, so the response body, which ends where \
                                     the connection does, may have been cut short"
                                ),
                            ));
                        }
                        self.done = true;
                        return Ok(false);
                    }
                    out.extend_from_slice(&chunk);
                    return Ok(true);
                }
            }
        }
    }
}
