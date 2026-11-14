//! The request handed to a handler.

use std::fmt;
use std::net::SocketAddr;
use std::ops::Range;
use std::rc::Rc;

use crate::body::{BodyStream, Pipe};
use crate::chunked::{Decoder, Progress};
use crate::parse::{find_head_end, header_lines, parse_head, Head};

pub(crate) enum BodyBytes {
    /// A range of the connection buffer (`Content-Length` bodies: no copy).
    InBuffer(Range<usize>),
    /// A decoded chunked body.
    Decoded(Vec<u8>),
    /// A body read as it arrives (see `Handler::stream_body`).
    Stream(Rc<Pipe>),
}

/// The HTTP version of a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Version {
    /// HTTP/1.0.
    Http10,
    /// HTTP/1.1.
    Http11,
    /// HTTP/2, over TLS (chosen through ALPN) or over plain TCP with prior
    /// knowledge.
    Http2,
}

impl Version {
    /// The version as it is written on the wire: `HTTP/1.0`, `HTTP/1.1`,
    /// `HTTP/2`.
    pub fn as_str(self) -> &'static str {
        match self {
            Version::Http10 => "HTTP/1.0",
            Version::Http11 => "HTTP/1.1",
            Version::Http2 => "HTTP/2",
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An HTTP request, normally with its body fully received.
///
/// A request shares the connection's receive buffer instead of copying out of
/// it, so creating one allocates nothing. It can be kept for as long as needed;
/// the connection only reuses the buffer once the request has been dropped.
///
/// For a handler that asked for it with
/// [`Handler::stream_body`](crate::Handler::stream_body), the body is not
/// received yet: [`body`](Self::body) is empty and
/// [`body_stream`](Self::body_stream) reads it.
pub struct Request {
    pub(crate) buf: Rc<Vec<u8>>,
    /// Where the head starts in `buf`.
    pub(crate) start: usize,
    pub(crate) head: Head,
    pub(crate) body: BodyBytes,
    pub(crate) peer: Option<SocketAddr>,
}

impl Request {
    /// Parses one complete request (head and body) from `raw`, as the server
    /// would: for testing handlers without a socket.
    ///
    /// ```
    /// use giong_http::Request;
    ///
    /// let req = Request::from_bytes(
    ///     b"POST /items?draft=1 HTTP/1.1\r\nHost: x\r\nContent-Length: 5\r\n\r\nhello",
    /// )
    /// .unwrap();
    /// assert_eq!((req.method(), req.path(), req.query()), ("POST", "/items", Some("draft=1")));
    /// assert_eq!(req.body(), b"hello");
    /// ```
    ///
    /// # Errors
    ///
    /// The status the server would answer with (`400` for a malformed head,
    /// a body shorter or longer than announced, or bytes after the request;
    /// `501` for an unsupported transfer coding...).
    pub fn from_bytes(raw: &[u8]) -> Result<Request, u16> {
        let end = find_head_end(raw, 0).ok_or(400u16)?;
        let head = parse_head(&raw[..end])?;
        let (body, total) = if head.chunked {
            let mut decoder = Decoder::new(usize::MAX);
            match decoder.feed(&raw[end..])? {
                Progress::Done(used) => (BodyBytes::Decoded(decoder.body), end + used),
                Progress::NeedMore => return Err(400),
            }
        } else {
            let len = usize::try_from(head.content_length.unwrap_or(0)).map_err(|_| 413u16)?;
            if raw.len() - end < len {
                return Err(400);
            }
            (BodyBytes::InBuffer(end..end + len), end + len)
        };
        if total != raw.len() {
            return Err(400);
        }
        Ok(Request {
            buf: Rc::new(raw.to_vec()),
            start: 0,
            head,
            body,
            peer: None,
        })
    }

    /// The same request, seen as coming from `addr`: for tests of code that
    /// reads [`peer_addr`](Self::peer_addr).
    pub fn with_peer_addr(mut self, addr: SocketAddr) -> Self {
        self.peer = Some(addr);
        self
    }

    fn head_bytes(&self) -> &[u8] {
        &self.buf[self.start..self.start + self.head.len]
    }

    fn head_str(&self, range: Range<usize>) -> &str {
        // The parser only accepts ASCII in the method and the target.
        std::str::from_utf8(&self.head_bytes()[range]).unwrap_or("")
    }

    /// The request method, e.g. `GET`. Case-sensitive, as sent.
    pub fn method(&self) -> &str {
        self.head_str(self.head.method.clone())
    }

    /// The request target exactly as sent, e.g. `/search?q=rust`.
    ///
    /// It can also be a whole URL (absolute form, `http://host/search`),
    /// which HTTP/1.1 servers must accept. The server refuses one whose
    /// scheme is not `http` or `https` or whose host is not the one in the
    /// `Host` header (`400`), so [`header("host")`](Self::header) names the
    /// host whatever the form, as it does over HTTP/2. `*` comes only with
    /// `OPTIONS`, and `host:port` with `CONNECT`.
    pub fn target(&self) -> &str {
        self.head_str(self.head.target.clone())
    }

    /// The path part of the target, without the query string. For a target in
    /// absolute form (`http://host/path`), the scheme and host are left out,
    /// and a URL with no path has the path `/`.
    pub fn path(&self) -> &str {
        let target = self.target();
        let target = match crate::parse::split_absolute(target.as_bytes()) {
            // `split_absolute` cuts at ASCII bytes, so `rest` is a suffix
            // of the target on a character boundary.
            Some((_, _, rest)) if !target.starts_with('/') => match rest.first() {
                Some(b'/') => &target[target.len() - rest.len()..],
                _ => "/",
            },
            _ => target,
        };
        target.split('?').next().unwrap_or(target)
    }

    /// The query string, without the leading `?`.
    pub fn query(&self) -> Option<&str> {
        self.target().split_once('?').map(|(_, query)| query)
    }

    /// The HTTP version the request came with.
    pub fn version(&self) -> Version {
        match (self.head.h2, self.head.minor) {
            (true, _) => Version::Http2,
            (false, 0) => Version::Http10,
            (false, _) => Version::Http11,
        }
    }

    /// `1` for HTTP/1.1, `0` for HTTP/1.0. An HTTP/2 request reports `1`:
    /// it can do everything HTTP/1.1 can that HTTP/1.0 cannot (keep the
    /// connection open, stream a body). [`version`](Self::version) tells
    /// HTTP/2 apart.
    pub fn version_minor(&self) -> u8 {
        self.head.minor
    }

    /// All header fields in the order they were sent, values trimmed.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &[u8])> {
        header_lines(&self.head_bytes()[self.head.headers.clone()])
            .map(|(name, value)| (std::str::from_utf8(name).unwrap_or(""), value))
    }

    /// The first value of the header `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// Like [`header`](Self::header), for values that are valid UTF-8.
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.header(name).and_then(|v| std::str::from_utf8(v).ok())
    }

    /// The address of the client that sent this request, as the connection
    /// sees it. `None` over a Unix socket.
    ///
    /// Behind a reverse proxy this is the proxy's address. The client's own
    /// address is then in a header such as `X-Forwarded-For`, which is only
    /// worth believing when it comes from a proxy you run.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer
    }

    /// The request body. Empty if the request has none, or if its body is
    /// read as a stream ([`body_stream`](Self::body_stream)).
    pub fn body(&self) -> &[u8] {
        match &self.body {
            BodyBytes::InBuffer(range) => &self.buf[range.clone()],
            BodyBytes::Decoded(body) => body,
            BodyBytes::Stream(_) => &[],
        }
    }

    /// The body, read as it arrives, when the handler's
    /// [`Handler::stream_body`](crate::Handler::stream_body) asked for it;
    /// `None` otherwise (the body is then in [`body`](Self::body)). Every
    /// call returns a handle to the same body.
    pub fn body_stream(&self) -> Option<BodyStream> {
        match &self.body {
            BodyBytes::Stream(pipe) => Some(BodyStream::new(pipe.clone())),
            _ => None,
        }
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method())
            .field("target", &self.target())
            .field("version", &self.version())
            .field("peer_addr", &self.peer)
            .field("body_len", &self.body().len())
            .field("body_stream", &matches!(self.body, BodyBytes::Stream(_)))
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_bytes_parses_heads_and_both_kinds_of_body() {
        let req =
            Request::from_bytes(b"GET /a/b?x=1 HTTP/1.1\r\nHost: h\r\nX-Id: 7\r\n\r\n").unwrap();
        assert_eq!(
            (req.method(), req.path(), req.query()),
            ("GET", "/a/b", Some("x=1"))
        );
        assert_eq!(req.header_str("x-id"), Some("7"));
        assert!(req.body().is_empty() && req.peer_addr().is_none());

        let chunked = b"POST / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(Request::from_bytes(chunked).unwrap().body(), b"abcde");

        let addr: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let req = Request::from_bytes(b"GET / HTTP/1.1\r\nHost: h\r\n\r\n").unwrap();
        assert_eq!(req.with_peer_addr(addr).peer_addr(), Some(addr));
    }

    #[test]
    fn an_absolute_target_gives_its_path_and_its_host_is_the_host_header() {
        let req = Request::from_bytes(
            b"GET http://example.com/a/b?x=1 HTTP/1.1\r\nHost: Example.com\r\n\r\n",
        )
        .unwrap();
        assert_eq!(req.target(), "http://example.com/a/b?x=1");
        assert_eq!((req.path(), req.query()), ("/a/b", Some("x=1")));
        assert_eq!(req.header_str("host"), Some("Example.com"));
        // The path ends where the query starts, even when the query has a
        // slash in it.
        let req = Request::from_bytes(
            b"GET http://example.com?to=/next HTTP/1.1\r\nHost: example.com\r\n\r\n",
        )
        .unwrap();
        assert_eq!((req.path(), req.query()), ("/", Some("to=/next")));
        let req =
            Request::from_bytes(b"GET HTTPS://example.com HTTP/1.1\r\nHost: example.com\r\n\r\n")
                .unwrap();
        assert_eq!((req.path(), req.query()), ("/", None));
    }

    #[test]
    fn from_bytes_refuses_what_the_server_refuses() {
        let cases: [&[u8]; 7] = [
            b"GET / HTTP/1.1\r\nHost: h\r\n",
            b"GET / HTTP/1.1\r\nHost: h\r\nContent-Length: 5\r\n\r\nabc",
            b"GET / HTTP/1.1\r\nHost: h\r\n\r\nextra",
            b"GET / HTTP/1.1\r\nHost : h\r\n\r\n",
            b"POST / HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nab",
            // The handler would see one host in Host and the front proxy
            // another in the target.
            b"GET http://internal.example/admin HTTP/1.1\r\nHost: public.example\r\n\r\n",
            b"GET foo://h/x HTTP/1.1\r\nHost: h\r\n\r\n",
        ];
        for raw in cases {
            assert_eq!(
                Request::from_bytes(raw).err(),
                Some(400),
                "{:?}",
                String::from_utf8_lossy(raw)
            );
        }
    }
}
