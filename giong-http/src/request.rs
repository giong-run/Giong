//! The request handed to a handler.

use std::fmt;
use std::net::SocketAddr;
use std::ops::Range;
use std::rc::Rc;

use crate::parse::{header_lines, Head};

pub(crate) enum BodyBytes {
    /// A range of the connection buffer (`Content-Length` bodies: no copy).
    InBuffer(Range<usize>),
    /// A decoded chunked body.
    Decoded(Vec<u8>),
}

/// An HTTP request with its body fully received.
///
/// A request shares the connection's receive buffer instead of copying out of
/// it, so creating one allocates nothing. It can be kept for as long as needed;
/// the connection only reuses the buffer once the request has been dropped.
pub struct Request {
    pub(crate) buf: Rc<Vec<u8>>,
    /// Where the head starts in `buf`.
    pub(crate) start: usize,
    pub(crate) head: Head,
    pub(crate) body: BodyBytes,
    pub(crate) peer: Option<SocketAddr>,
}

impl Request {
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
    pub fn target(&self) -> &str {
        self.head_str(self.head.target.clone())
    }

    /// The path part of the target, without the query string. For a target in
    /// absolute form (`http://host/path`), the scheme and host are left out.
    pub fn path(&self) -> &str {
        let target = self.target();
        let target = match target.find("://") {
            Some(i) if !target.starts_with('/') => {
                let rest = &target[i + 3..];
                rest.find('/').map_or("/", |slash| &rest[slash..])
            }
            _ => target,
        };
        target.split('?').next().unwrap_or(target)
    }

    /// The query string, without the leading `?`.
    pub fn query(&self) -> Option<&str> {
        self.target().split_once('?').map(|(_, query)| query)
    }

    /// `1` for HTTP/1.1, `0` for HTTP/1.0.
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

    /// The request body. Empty if the request has none.
    pub fn body(&self) -> &[u8] {
        match &self.body {
            BodyBytes::InBuffer(range) => &self.buf[range.clone()],
            BodyBytes::Decoded(body) => body,
        }
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method())
            .field("target", &self.target())
            .field("version_minor", &self.version_minor())
            .field("peer_addr", &self.peer)
            .field("body_len", &self.body().len())
            .finish()
    }
}
