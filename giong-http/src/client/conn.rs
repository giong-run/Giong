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

/// Where the bytes of a response come from: a connection's socket or, for
/// the `response_body` fuzz target and its randomised test, bytes in
/// memory. Reading a response ([`read_answer`], [`Body::read`]) needs
/// nothing else, so the code that decides where a body ends, and whether
/// the connection can carry another request, runs unchanged on both.
pub(crate) trait Source {
    /// The next bytes; empty at the end of the stream.
    async fn recv(&self) -> io::Result<Chunk>;

    /// Whether an end of stream just received can be trusted to be the
    /// server's (see `Stream::ended_cleanly`).
    fn ended_cleanly(&self) -> bool;
}

impl Source for Conn {
    async fn recv(&self) -> io::Result<Chunk> {
        self.stream.recv().await
    }

    fn ended_cleanly(&self) -> bool {
        self.stream.ended_cleanly()
    }
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
        read_answer(self, head_request, max_head_size, what).await
    }
}

/// Reads the head of a response from `source`, after any 1xx
/// (informational) responses, and sets up the reading of its body.
///
/// Not cancel safe: dropped, it loses the bytes it has received, so the
/// connection cannot carry another request.
pub(crate) async fn read_answer(
    source: &impl Source,
    head_request: bool,
    max_head_size: usize,
    what: &str,
) -> Result<Answer, Failure> {
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
        if (scanned..buf.len()).any(|at| buf[at] == b'\n' && (at == 0 || buf[at - 1] != b'\r')) {
            return Err(protocol(
                what,
                "a line of the head ends in a bare line feed",
            ));
        }
        scanned = buf.len();
        let chunk = recv_part(source, what, received_any, "the response head").await?;
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
async fn recv_part(
    source: &impl Source,
    what: &str,
    received_any: bool,
    part: &str,
) -> Result<Chunk, Failure> {
    let fail = |e: Error| {
        if received_any {
            Failure::Other(e)
        } else {
            Failure::NothingReceived(e)
        }
    };
    match source.recv().await {
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
        conn: &impl Source,
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
                        let chunk = recv_part(conn, what, true, "the response body")
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
                        let chunk = recv_part(conn, what, true, "the chunked response body")
                            .await
                            .map_err(Failure::into_error)?;
                        self.buf.extend_from_slice(&chunk);
                    }
                    Err(_) => {
                        return Err(protocol(
                            what,
                            "the chunked body is malformed (a chunk size line, the end of a \
                             chunk or a trailer line)",
                        )
                        .into_error());
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
                    let chunk = conn.recv().await.map_err(|e| {
                        Error::io(
                            ErrorKind::Closed,
                            format!("{what}: reading the response body failed"),
                            e,
                        )
                    })?;
                    if chunk.is_empty() {
                        // Without close_notify the body may have been cut
                        // short on the way (a truncation attack).
                        if !conn.ended_cleanly() {
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

/// What the `response_body` fuzz target checks: a response read from bytes
/// in memory by the code that reads one on a connection, against what RFC
/// 9112 (section 6.3) says the bytes hold, worked out on the whole input
/// at once.
#[cfg(any(test, fuzzing))]
pub(crate) mod fuzzing {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::future::Future;
    use std::io;
    use std::pin::pin;
    use std::sync::Arc;
    use std::task::{Context, Poll, Waker};

    use super::{read_answer, Chunk, Noop, Source};
    use crate::chunked::{Decoder, Progress};
    use crate::client::error::ErrorKind;
    use crate::client::head::{parse_head, Head};
    use crate::parse::{find_head_end, trim};

    /// A connection's socket in memory: hands out `pieces` in turn, then
    /// the end of the stream, and counts the bytes it handed out.
    struct Memory {
        pieces: RefCell<VecDeque<Vec<u8>>>,
        handed_out: Cell<usize>,
        /// The stream ends with an error (a reset) instead of a close.
        reset: bool,
        /// The close came with TLS `close_notify` (or over plain TCP).
        clean: bool,
    }

    impl Source for Memory {
        async fn recv(&self) -> io::Result<Chunk> {
            match self.pieces.borrow_mut().pop_front() {
                Some(piece) => {
                    self.handed_out.set(self.handed_out.get() + piece.len());
                    Ok(Chunk::Owned(piece))
                }
                None if self.reset => Err(io::ErrorKind::ConnectionReset.into()),
                None => Ok(Chunk::Owned(Vec::new())),
            }
        }

        fn ended_cleanly(&self) -> bool {
            self.clean
        }
    }

    /// Runs `future`, which reads from memory and so must never wait.
    fn now<F: Future>(future: F) -> F::Output {
        let waker = Waker::from(Arc::new(Noop));
        match pin!(future).poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("reading a response from memory waited"),
        }
    }

    /// `input` in non-empty pieces of 1 to 64 bytes, their sizes drawn
    /// from `seed`; in one piece for seed 0.
    fn pieces(input: &[u8], seed: u8) -> VecDeque<Vec<u8>> {
        let mut out = VecDeque::new();
        if seed == 0 {
            if !input.is_empty() {
                out.push_back(input.to_vec());
            }
            return out;
        }
        let mut state = u32::from(seed);
        let mut rest = input;
        while !rest.is_empty() {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let n = (1 + state as usize % 64).min(rest.len());
            out.push_back(rest[..n].to_vec());
            rest = &rest[n..];
        }
        out
    }

    /// The final head in `input`, after any 1xx ones, if the client can
    /// take every head on the way: whole, valid, within `max_head` bytes,
    /// no 101 and at most 32 1xx responses. Its start, its end and the
    /// head.
    fn final_head(input: &[u8], max_head: usize) -> Option<(usize, usize, Head)> {
        let mut at = 0;
        for _ in 0..=32 {
            let len = find_head_end(&input[at..], 0)?;
            if len > max_head {
                return None;
            }
            let head = parse_head(&input[at..at + len]).ok()?;
            if head.status == 101 {
                return None;
            }
            if head.status >= 200 {
                return Some((at, at + len, head));
            }
            at += len;
        }
        None
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Framing {
        None,
        Length(u64),
        Chunked,
        UntilClose,
    }

    /// How the body after `head` is framed, and whether the connection may
    /// carry another request after it, from the header fields themselves.
    fn framing(head: &Head, bytes: &[u8], head_request: bool) -> (Framing, bool) {
        let fields = || {
            head.fields
                .iter()
                .map(|(name, value)| (&bytes[name.clone()], &bytes[value.clone()]))
        };
        let named = |name: &'static [u8]| {
            fields()
                .filter(move |(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, value)| value)
        };
        let length = named(b"content-length").next().map(|value| {
            value
                .iter()
                .fold(0u64, |n, &d| n * 10 + u64::from(d - b'0'))
        });
        // The head parser refuses any transfer coding but chunked.
        let chunked = named(b"transfer-encoding").next().is_some();
        let tokens: Vec<&[u8]> = named(b"connection")
            .flat_map(|value| value.split(|&b| b == b',').map(trim))
            .collect();
        let says = |token: &[u8]| tokens.iter().any(|t| t.eq_ignore_ascii_case(token));
        let close = if head.minor == 0 {
            !says(b"keep-alive") || says(b"close")
        } else {
            says(b"close")
        };
        // A length and chunked: the chunked framing wins, and the
        // connection is not used again (RFC 9112, section 6.3).
        let close = close || (chunked && length.is_some());
        let framing = if head_request || head.status == 204 || head.status == 304 {
            Framing::None
        } else if chunked {
            Framing::Chunked
        } else if let Some(n) = length {
            Framing::Length(n)
        } else {
            Framing::UntilClose
        };
        (framing, close)
    }

    /// What reading the body must give: its bytes and where it ends in the
    /// input, or the kind of error and the bytes that may come before it.
    enum Outcome {
        Body(Vec<u8>, usize),
        Fails(ErrorKind, Vec<u8>),
    }

    fn outcome(input: &[u8], start: usize, framing: Framing, clean: bool) -> Outcome {
        let rest = &input[start..];
        match framing {
            Framing::None => Outcome::Body(Vec::new(), start),
            Framing::Length(n) => match usize::try_from(n) {
                Ok(n) if n <= rest.len() => Outcome::Body(rest[..n].to_vec(), start + n),
                _ => Outcome::Fails(ErrorKind::Closed, rest.to_vec()),
            },
            Framing::Chunked => {
                let mut decoder = Decoder::new(usize::MAX);
                match decoder.feed(rest) {
                    Ok(Progress::Done(used)) => Outcome::Body(decoder.body, start + used),
                    Ok(Progress::NeedMore) => Outcome::Fails(ErrorKind::Closed, decoder.body),
                    Err(_) => Outcome::Fails(ErrorKind::Protocol, decoder.body),
                }
            }
            Framing::UntilClose if clean => Outcome::Body(rest.to_vec(), input.len()),
            Framing::UntilClose => Outcome::Fails(ErrorKind::Closed, rest.to_vec()),
        }
    }

    /// Byte 0: bit 0 the request was `HEAD`; bit 1 read the body as
    /// `read_whole` does (taking the head's buffer first); bit 2 the close
    /// has no `close_notify`; bit 3 the stream ends with an error; the
    /// upper bits pick the head limit. Byte 1: seed for the piece sizes.
    /// Then what the server sends.
    ///
    /// The head is the one the bytes hold, or refused when they hold none
    /// the client can take; the body is exactly the bytes its framing
    /// delimits, given out in non-empty pieces, or the error its framing
    /// calls for; and the connection is reusable exactly when the server
    /// did not close it, the body had a framing of its own, and the bytes
    /// received end where the body does (a byte more would be read as the
    /// next response). Returns `true` if the whole body was read.
    pub(crate) fn check_response_body(data: &[u8]) -> bool {
        let [flags, seed, input @ ..] = data else {
            return false;
        };
        let head_request = flags & 1 != 0;
        let take_whole = flags & 2 != 0;
        let reset = flags & 8 != 0;
        let clean = flags & 4 == 0 && !reset;
        let max_head = [64, 256, 64 * 1024][usize::from(flags >> 4) % 3];
        let source = Memory {
            pieces: RefCell::new(pieces(input, *seed)),
            handed_out: Cell::new(0),
            reset,
            clean: flags & 4 == 0,
        };
        let what = "GET http://fuzz.example/";

        let answer = now(read_answer(&source, head_request, max_head, what));
        let expected = final_head(input, max_head);
        let (mut answer, (start, end, head)) = match (answer, expected) {
            (Ok(answer), Some(expected)) => (answer, expected),
            (Err(failure), None) => {
                let e = failure.into_error();
                assert!(
                    matches!(
                        e.kind(),
                        ErrorKind::Protocol | ErrorKind::TooLarge | ErrorKind::Closed
                    ),
                    "{e}"
                );
                assert!(e.to_string().contains(what), "{e}");
                return false;
            }
            (Ok(answer), None) => panic!("a head the client should refuse: {:?}", answer.head),
            (Err(failure), Some(_)) => panic!("a valid head refused: {}", failure.into_error()),
        };
        assert_eq!(answer.head, head);
        let head_bytes = &input[start..end];
        assert_eq!(answer.body.head_bytes(), head_bytes);
        let (framing, close) = framing(&head, head_bytes, head_request);
        match framing {
            Framing::None => assert_eq!(answer.body.length(), Some(0)),
            Framing::Length(n) => assert_eq!(answer.body.length(), Some(n)),
            Framing::Chunked | Framing::UntilClose => assert_eq!(answer.body.length(), None),
        }

        let body = &mut answer.body;
        let mut got = Vec::new();
        if take_whole {
            match body.take_with_head() {
                Some(data) => {
                    assert!(matches!(framing, Framing::Length(_)), "{framing:?}");
                    assert_eq!(&data[..head_bytes.len()], head_bytes);
                    got.extend_from_slice(&data[head_bytes.len()..]);
                    assert!(body.head_bytes().is_empty());
                }
                None => assert!(!matches!(framing, Framing::Length(_)), "{framing:?}"),
            }
        }
        let result = loop {
            let before = got.len();
            match now(body.read(&source, &mut got, what)) {
                Ok(true) => assert!(got.len() > before, "a piece with nothing in it"),
                Ok(false) => {
                    assert_eq!(got.len(), before, "bytes with the end");
                    break Ok(());
                }
                Err(e) => {
                    assert_eq!(got.len(), before, "bytes with an error");
                    break Err(e);
                }
            }
        };

        match (outcome(input, end, framing, clean), result) {
            (Outcome::Body(bytes, body_end), Ok(())) => {
                assert_eq!(got, bytes, "{framing:?}");
                assert!(body.is_done());
                // Read to its end, it stays ended.
                let mut more = Vec::new();
                assert!(!now(body.read(&source, &mut more, what)).unwrap());
                assert!(more.is_empty());
                let exact = source.handed_out.get() == body_end;
                let reusable = !close && framing != Framing::UntilClose && exact;
                assert_eq!(
                    body.reusable(),
                    reusable,
                    "{framing:?}, close {close}, received {} bytes, the body ends at {body_end}",
                    source.handed_out.get()
                );
                true
            }
            (Outcome::Fails(kind, before), Err(e)) => {
                assert_eq!(e.kind(), kind, "{e}");
                assert!(e.to_string().contains(what), "{e}");
                assert!(
                    before.starts_with(&got),
                    "{framing:?}: more than the body had"
                );
                assert!(!body.reusable());
                false
            }
            (Outcome::Body(..), Err(e)) => panic!("{framing:?}: a valid body failed: {e}"),
            (Outcome::Fails(kind, _), Ok(())) => {
                panic!("{framing:?}: a body that should fail with {kind:?} was read: {got:?}")
            }
        }
    }

    #[test]
    fn bodies_end_at_their_framing_and_bytes_after_them_spoil_the_connection() {
        let check = |flags: u8, seed: u8, input: &[u8]| {
            let mut data = vec![flags, seed];
            data.extend_from_slice(input);
            check_response_body(&data)
        };
        for seed in [0, 1, 7, 200] {
            for flags in [0, 2, 0x20] {
                assert!(check(
                    flags,
                    seed,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello"
                ));
                assert!(check(
                    flags,
                    seed,
                    b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n"
                ));
                assert!(check(
                    flags,
                    seed,
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\nX: 1\r\n\r\n"
                ));
                assert!(check(flags, seed, b"HTTP/1.0 200 OK\r\n\r\nto the end"));
                // Bytes after the body: read, but the connection is not reused.
                assert!(check(
                    flags,
                    seed,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokHTTP/1.1 200 OK\r\n\r\n"
                ));
                assert!(check(
                    flags,
                    seed,
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nX"
                ));
                assert!(check(
                    flags | 1,
                    seed,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
                ));
                // Cut short, malformed, or closed without close_notify.
                assert!(!check(
                    flags,
                    seed,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort"
                ));
                assert!(!check(
                    flags | 8,
                    seed,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort"
                ));
                assert!(!check(
                    flags,
                    seed,
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\nno colon\r\n\r\n"
                ));
                assert!(!check(flags | 4, seed, b"HTTP/1.1 200 OK\r\n\r\ncut?"));
                assert!(!check(
                    flags,
                    seed,
                    b"HTTP/1.1 101 Switching Protocols\r\n\r\n"
                ));
                assert!(!check(
                    flags,
                    seed,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n"
                ));
            }
        }
    }
}
