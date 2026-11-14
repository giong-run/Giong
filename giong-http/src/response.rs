//! The response a handler returns, and its wire format.

use std::any::Any;
use std::borrow::Cow;
use std::fmt;
use std::io;
use std::mem;
use std::rc::Rc;

use crate::body::{BodySender, StreamBody};
use crate::parse::{is_field_value, is_token_str};

/// A response body.
#[derive(Debug, Default)]
#[non_exhaustive]
pub enum Body {
    /// No body.
    #[default]
    Empty,
    /// Bytes that live for the whole program: no allocation, and from 32 KiB
    /// on they are sent from where they are, without a copy.
    Static(&'static [u8]),
    /// Owned bytes.
    Owned(Vec<u8>),
    /// A body written piece by piece as it is produced: see
    /// [`channel`](Self::channel).
    Stream(StreamBody),
    /// A range of an open file: see [`file`](Self::file). Over plain TCP
    /// and kernel TLS the kernel sends it straight from its page cache
    /// (`sendfile`); elsewhere it is read and written in pieces.
    File(FileBody),
}

/// A range of an open file as a response body: see [`Body::file`] and
/// [`Body::file_range`].
#[derive(Clone, Debug)]
pub struct FileBody {
    pub(crate) file: Rc<giong::fs::File>,
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

impl FileBody {
    /// The file.
    pub fn file(&self) -> &giong::fs::File {
        &self.file
    }

    /// Where the range starts in the file.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// The length of the range in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` if the range is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl Body {
    /// A body that is written as it is produced, and the sender that
    /// produces it: for a body too large to hold (a file, an export) or not
    /// known in full when the response starts (server-sent events).
    ///
    /// ```no_run
    /// use giong_http::{Body, Request, Response};
    ///
    /// async fn events(_req: Request) -> Response {
    ///     let (tx, body) = Body::channel();
    ///     drop(giong::spawn(async move {
    ///         for n in 0.. {
    ///             let event = format!("data: tick {n}\n\n");
    ///             if tx.send(event).await.is_err() {
    ///                 break; // The client went away.
    ///             }
    ///             giong::time::sleep(std::time::Duration::from_secs(1)).await;
    ///         }
    ///     }));
    ///     Response::new(200)
    ///         .header("Content-Type", "text/event-stream")
    ///         .header("Cache-Control", "no-store")
    ///         .body(body)
    /// }
    /// ```
    ///
    /// The response goes out with `Transfer-Encoding: chunked`, its head as
    /// soon as the handler returns and each piece as soon as it is sent.
    /// Dropping the [`BodySender`] ends the body. An HTTP/1.0 client gets
    /// the body without a length, and the connection closes after it.
    ///
    /// The server waits for the producer without a time limit (an event
    /// stream may be quiet for a long time); each piece must be written
    /// within [`Server::write_timeout`](crate::Server::write_timeout). A
    /// client that goes away is noticed at the next write, and the sender's
    /// [`send`](BodySender::send) fails from then on.
    pub fn channel() -> (BodySender, Body) {
        let (tx, body) = crate::body::channel();
        (tx, Body::Stream(body))
    }

    /// The whole of an open file as the body, its length taken from the
    /// open file. The file is shared, so one open file can serve any number
    /// of responses at once (pass an `Rc<File>` to keep it open between
    /// requests).
    ///
    /// Over plain TCP and kernel TLS the server sends the file with
    /// `sendfile`: from the page cache, without copying it into the
    /// program; that is the fastest way to serve files over HTTPS (see
    /// giong-tls' kernel TLS). Over TLS in the process, it is read and
    /// encrypted in pieces of 256 KiB.
    ///
    /// ```no_run
    /// use giong_http::{Body, Request, Response};
    ///
    /// async fn report(_req: Request) -> Response {
    ///     let Ok(file) = giong::fs::File::open("report.pdf").await else {
    ///         return Response::new(404);
    ///     };
    ///     match Body::file(file) {
    ///         Ok(body) => Response::new(200).header("Content-Type", "application/pdf").body(body),
    ///         Err(_) => Response::new(500),
    ///     }
    /// }
    /// ```
    ///
    /// If the file is shorter than its announced length when it is sent
    /// (truncated in between), the connection is closed after what it had,
    /// since the client was promised more (`GIONG-E0201`). Replace files
    /// that are served by renaming a new file over them.
    pub fn file(file: impl Into<Rc<giong::fs::File>>) -> io::Result<Body> {
        let file = file.into();
        let len = file.len()?;
        Ok(Body::file_range(file, 0, len))
    }

    /// `len` bytes of an open file from `offset`, as for a range request.
    /// See [`file`](Self::file). Nothing is checked until the response is
    /// sent: then, if the file ends before `offset + len`, the connection
    /// is closed after what it had (over HTTP/2, the stream is reset).
    pub fn file_range(file: impl Into<Rc<giong::fs::File>>, offset: u64, len: u64) -> Body {
        Body::File(FileBody {
            file: file.into(),
            offset,
            len,
        })
    }

    /// The bytes of the body. Empty for a [`Stream`](Self::Stream) and a
    /// [`File`](Self::File), whose bytes are not in memory.
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Body::Empty | Body::Stream(_) | Body::File(_) => &[],
            Body::Static(bytes) => bytes,
            Body::Owned(bytes) => bytes,
        }
    }

    /// The length of the body in bytes: of its bytes, or of a file's range;
    /// 0 for a [`Stream`](Self::Stream).
    pub fn len(&self) -> usize {
        match self {
            Body::File(f) => usize::try_from(f.len).unwrap_or(usize::MAX),
            _ => self.as_bytes().len(),
        }
    }

    /// `true` if the body has no bytes. A [`Stream`](Self::Stream) is not
    /// empty: its bytes are not known yet.
    pub fn is_empty(&self) -> bool {
        !self.is_stream() && self.len() == 0
    }

    /// `true` for a body written as it is produced.
    pub fn is_stream(&self) -> bool {
        matches!(self, Body::Stream(_))
    }

    /// `true` for a range of a file.
    pub fn is_file(&self) -> bool {
        matches!(self, Body::File(_))
    }

    /// The bytes as a buffer the kernel can send from, without a copy.
    /// Empty for a stream and a file, which the server sends otherwise.
    pub(crate) fn into_cow(self) -> Cow<'static, [u8]> {
        match self {
            Body::Empty | Body::Stream(_) | Body::File(_) => Cow::Borrowed(&[]),
            Body::Static(bytes) => Cow::Borrowed(bytes),
            Body::Owned(bytes) => Cow::Owned(bytes),
        }
    }
}

impl From<&'static str> for Body {
    fn from(s: &'static str) -> Self {
        Body::Static(s.as_bytes())
    }
}

impl From<&'static [u8]> for Body {
    fn from(s: &'static [u8]) -> Self {
        Body::Static(s)
    }
}

impl From<String> for Body {
    fn from(s: String) -> Self {
        Body::Owned(s.into_bytes())
    }
}

impl From<Vec<u8>> for Body {
    fn from(v: Vec<u8>) -> Self {
        Body::Owned(v)
    }
}

/// An HTTP response.
///
/// ```
/// use giong_http::Response;
///
/// let ok = Response::json(r#"{"ok":true}"#);
/// let missing = Response::new(404).header("Cache-Control", "no-store").body("not found");
/// # let _ = (ok, missing);
/// ```
#[derive(Debug)]
pub struct Response {
    status: u16,
    /// Extra header lines, already in wire format.
    headers: Vec<u8>,
    body: Body,
    extensions: Extensions,
}

/// Values attached to a response for the code around the handler; never
/// sent.
#[derive(Default)]
struct Extensions(Vec<Box<dyn Any>>);

impl fmt::Debug for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} extension(s)", self.0.len())
    }
}

impl Response {
    /// A response with the given status code and no body.
    ///
    /// # Panics
    ///
    /// Panics if `status` is not in `200..=599`. Interim (1xx) responses are
    /// sent by the server itself.
    pub fn new(status: u16) -> Self {
        assert!(
            (200..=599).contains(&status),
            "giong-http: invalid status code {status}"
        );
        Self {
            status,
            headers: Vec::new(),
            body: Body::Empty,
            extensions: Extensions::default(),
        }
    }

    /// `200 OK` with a `text/plain; charset=utf-8` body.
    pub fn text(body: impl Into<Body>) -> Self {
        Self::new(200)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(body)
    }

    /// `200 OK` with an `application/json` body.
    pub fn json(body: impl Into<Body>) -> Self {
        Self::new(200)
            .header("Content-Type", "application/json")
            .body(body)
    }

    /// Adds a header field.
    ///
    /// `Content-Length`, `Transfer-Encoding`, `Connection` and `Date` are
    /// managed by the server; setting them here is ignored.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a valid header name or `value` contains control
    /// characters. Building headers from untrusted input without checking it
    /// would otherwise allow response splitting.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        assert!(
            is_token_str(name.as_bytes()),
            "giong-http: invalid header name {name:?}"
        );
        assert!(
            is_field_value(value.as_bytes()),
            "giong-http: invalid value for header {name}: {value:?}"
        );
        let managed = ["content-length", "transfer-encoding", "connection", "date"];
        if !managed.iter().any(|m| name.eq_ignore_ascii_case(m)) {
            self.headers.extend_from_slice(name.as_bytes());
            self.headers.extend_from_slice(b": ");
            self.headers.extend_from_slice(value.trim().as_bytes());
            self.headers.extend_from_slice(b"\r\n");
        }
        self
    }

    /// Sets the body.
    pub fn body(mut self, body: impl Into<Body>) -> Self {
        self.body = body.into();
        self
    }

    /// The status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The header fields added with [`header`](Self::header), in order.
    /// Those the server manages (`Content-Length`, `Date`, ...) are not
    /// among them.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> {
        // Written by `header` only: a token, ": ", a value without CR or LF.
        std::str::from_utf8(&self.headers)
            .unwrap_or("")
            .split_terminator("\r\n")
            .filter_map(|line| line.split_once(": "))
    }

    /// The first value of the header field `name` (case-insensitive), as
    /// added with [`header`](Self::header).
    pub fn get_header(&self, name: &str) -> Option<&str> {
        self.headers()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// Removes every header field named `name` (case-insensitive).
    pub fn remove_header(mut self, name: &str) -> Self {
        if self.get_header(name).is_none() {
            return self;
        }
        let mut kept = Vec::with_capacity(self.headers.len());
        for (n, value) in self.headers() {
            if !n.eq_ignore_ascii_case(name) {
                kept.extend_from_slice(n.as_bytes());
                kept.extend_from_slice(b": ");
                kept.extend_from_slice(value.as_bytes());
                kept.extend_from_slice(b"\r\n");
            }
        }
        self.headers = kept;
        self
    }

    /// The body's bytes. Empty for a body written as it is produced (see
    /// [`is_streaming`](Self::is_streaming)) and for a file
    /// ([`Body::file`]), whose bytes are not in memory.
    pub fn body_bytes(&self) -> &[u8] {
        self.body.as_bytes()
    }

    /// `true` if the body is written as it is produced
    /// ([`Body::channel`]): its bytes are not known yet, so code around the
    /// handler (compression, an access log) should leave it alone.
    pub fn is_streaming(&self) -> bool {
        self.body.is_stream()
    }

    /// `true` if the body is a range of a file ([`Body::file`]): its length
    /// is known, its bytes are not in memory.
    pub fn body_is_file(&self) -> bool {
        self.body.is_file()
    }

    /// Takes the body out, leaving the response without one.
    pub fn take_body(&mut self) -> Body {
        mem::take(&mut self.body)
    }

    /// Attaches `value`, replacing a value of the same type attached before.
    ///
    /// Extensions are never sent. They carry information from a handler to
    /// the code that wraps it, such as the error a framework turned into
    /// this response, for its access log to report.
    pub fn with_extension<T: 'static>(mut self, value: T) -> Self {
        let list = &mut self.extensions.0;
        match list.iter_mut().find(|v| v.is::<T>()) {
            Some(slot) => *slot = Box::new(value),
            None => list.push(Box::new(value)),
        }
        self
    }

    /// The value of type `T` attached with
    /// [`with_extension`](Self::with_extension), if any.
    pub fn extension<T: 'static>(&self) -> Option<&T> {
        self.extensions.0.iter().find_map(|v| v.downcast_ref())
    }

    /// Appends the response in wire format to `out`.
    pub(crate) fn write_to(&self, out: &mut Vec<u8>, minor: u8, keep_alive: bool, head_only: bool) {
        let body = self.body.as_bytes();
        out.reserve(128 + self.headers.len() + body.len());
        self.write_head(out, minor, keep_alive);
        if self.sends_body(head_only) {
            out.extend_from_slice(body);
        }
    }

    /// `true` if the body goes on the wire: not for `HEAD`, 204 or 304.
    pub(crate) fn sends_body(&self, head_only: bool) -> bool {
        !head_only && !self.bodiless()
    }

    /// 204 and 304 never carry a body, so they get no length either.
    fn bodiless(&self) -> bool {
        matches!(self.status, 204 | 304)
    }

    /// The length of the body in bytes, as `Content-Length` gives it: of
    /// its bytes, or of a file's range; 0 for a streamed body.
    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    /// Appends the status line and the header fields, up to and including the
    /// blank line, to `out`. A streamed body gets `Transfer-Encoding:
    /// chunked` over HTTP/1.1, and no length over HTTP/1.0 (the caller then
    /// closes the connection after it).
    pub(crate) fn write_head(&self, out: &mut Vec<u8>, minor: u8, keep_alive: bool) {
        out.reserve(128 + self.headers.len());
        out.extend_from_slice(if minor == 0 {
            b"HTTP/1.0 "
        } else {
            b"HTTP/1.1 "
        });
        push_number(out, self.status as usize);
        out.push(b' ');
        out.extend_from_slice(reason(self.status).as_bytes());
        out.extend_from_slice(b"\r\nDate: ");
        out.extend_from_slice(&crate::date::now());
        out.extend_from_slice(b"\r\n");
        match (self.bodiless(), self.body.is_stream()) {
            (true, _) => {}
            (false, false) => {
                out.extend_from_slice(b"Content-Length: ");
                push_number(out, self.body_len());
                out.extend_from_slice(b"\r\n");
            }
            (false, true) if minor == 1 => {
                out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
            }
            (false, true) => {}
        }
        out.extend_from_slice(&self.headers);
        if !keep_alive {
            out.extend_from_slice(b"Connection: close\r\n");
        } else if minor == 0 {
            out.extend_from_slice(b"Connection: keep-alive\r\n");
        }
        out.extend_from_slice(b"\r\n");
    }
}

fn push_number(out: &mut Vec<u8>, mut n: usize) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&digits[at..]);
}

/// The reason phrase for a status code, as the server sends it: `"Not Found"`
/// for 404. Codes without a phrase of their own get their class's: `"Client
/// Error"` for 499.
pub fn reason_phrase(status: u16) -> &'static str {
    reason(status)
}

pub(crate) fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => match status / 100 {
            2 => "OK",
            3 => "Redirection",
            4 => "Client Error",
            _ => "Server Error",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(resp: &Response, minor: u8, keep_alive: bool, head_only: bool) -> String {
        let mut out = Vec::new();
        resp.write_to(&mut out, minor, keep_alive, head_only);
        let text = String::from_utf8(out).unwrap();
        // The date changes; check its shape and cut it out.
        let start = text.find("Date: ").unwrap();
        let end = start + text[start..].find("\r\n").unwrap() + 2;
        assert_eq!(end - start, 6 + 29 + 2);
        format!("{}{}", &text[..start], &text[end..])
    }

    #[test]
    fn serialises_status_headers_and_body() {
        let resp = Response::json(r#"{"a":1}"#).header("X-Id", " 7 ");
        assert_eq!(
            wire(&resp, 1, true, false),
            "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nContent-Type: application/json\r\nX-Id: 7\r\n\r\n{\"a\":1}"
        );
        assert_eq!(
            wire(&Response::new(404), 1, false, false),
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(
            wire(&Response::text(vec![b'x'; 12345]), 0, true, true),
            "HTTP/1.0 200 OK\r\nContent-Length: 12345\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: keep-alive\r\n\r\n"
        );
        assert_eq!(
            wire(&Response::new(204).body("ignored"), 1, true, false),
            "HTTP/1.1 204 No Content\r\n\r\n"
        );
        assert_eq!(
            wire(&Response::new(599), 1, true, false),
            "HTTP/1.1 599 Server Error\r\nContent-Length: 0\r\n\r\n"
        );
    }

    #[test]
    fn managed_headers_cannot_be_overridden() {
        let resp = Response::new(200)
            .header("content-length", "999")
            .header("Connection", "keep-alive")
            .header("Transfer-Encoding", "chunked")
            .header("Date", "yesterday")
            .body("ok");
        assert_eq!(
            wire(&resp, 1, true, false),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"
        );
    }

    #[test]
    fn headers_can_be_read_back_and_removed() {
        let resp = Response::new(200)
            .header("ETag", "\"v1\"")
            .header("Vary", "Origin")
            .header("vary", "Accept-Encoding")
            .header("Content-Length", "5");
        assert_eq!(resp.get_header("etag"), Some("\"v1\""));
        assert_eq!(resp.get_header("VARY"), Some("Origin"));
        assert_eq!(resp.get_header("Content-Length"), None);
        let names: Vec<_> = resp.headers().collect();
        assert_eq!(
            names,
            [
                ("ETag", "\"v1\""),
                ("Vary", "Origin"),
                ("vary", "Accept-Encoding")
            ]
        );
        let resp = resp.remove_header("Vary").remove_header("missing");
        assert_eq!(resp.headers().collect::<Vec<_>>(), [("ETag", "\"v1\"")]);
        assert_eq!(
            wire(&resp, 1, true, false),
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nETag: \"v1\"\r\n\r\n"
        );
    }

    #[test]
    fn extensions_are_kept_by_type_and_never_sent() {
        #[derive(Debug, PartialEq)]
        struct Cause(&'static str);
        let resp = Response::text("x")
            .with_extension(Cause("first"))
            .with_extension(7u32)
            .with_extension(Cause("second"));
        assert_eq!(resp.extension::<Cause>(), Some(&Cause("second")));
        assert_eq!(resp.extension::<u32>(), Some(&7));
        assert_eq!(resp.extension::<String>(), None);
        assert!(!wire(&resp, 1, true, false).contains("second"));
    }

    #[test]
    fn the_body_can_be_read_and_taken() {
        let mut resp = Response::text("hello");
        assert_eq!(resp.body_bytes(), b"hello");
        let body = resp.take_body();
        assert_eq!(
            (body.as_bytes(), body.len(), body.is_empty()),
            (&b"hello"[..], 5, false)
        );
        assert!(resp.body_bytes().is_empty());
    }

    #[test]
    fn a_streamed_body_is_chunked_over_http_1_1_and_unframed_over_1_0() {
        let (tx, body) = Body::channel();
        assert!(body.is_stream() && !body.is_empty() && body.as_bytes().is_empty());
        let resp = Response::new(200)
            .header("Content-Type", "text/plain")
            .body(body);
        assert!(resp.is_streaming() && resp.body_bytes().is_empty());
        assert_eq!(
            wire(&resp, 1, true, false),
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/plain\r\n\r\n"
        );
        assert_eq!(
            wire(&resp, 0, false, false),
            "HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n"
        );
        let resp = Response::new(304).body(Body::channel().1);
        assert_eq!(
            wire(&resp, 1, true, false),
            "HTTP/1.1 304 Not Modified\r\n\r\n"
        );
        assert!(!tx.is_closed());
    }

    #[test]
    #[should_panic(expected = "invalid value for header")]
    fn header_values_cannot_split_the_response() {
        let _ = Response::new(200).header("X-User", "a\r\nSet-Cookie: stolen=1");
    }

    #[test]
    #[should_panic(expected = "invalid header name")]
    fn header_names_must_be_tokens() {
        let _ = Response::new(200).header("Bad Name", "1");
    }

    #[test]
    #[should_panic(expected = "invalid status code")]
    fn interim_status_codes_are_rejected() {
        let _ = Response::new(100);
    }
}
