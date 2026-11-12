//! The response a handler returns, and its wire format.

use std::borrow::Cow;
use std::mem;

use crate::parse::{is_field_value, is_token_str};

/// A response body.
#[derive(Debug, Default)]
pub enum Body {
    /// No body.
    #[default]
    Empty,
    /// Bytes that live for the whole program: no allocation, and from 32 KiB
    /// on they are sent from where they are, without a copy.
    Static(&'static [u8]),
    /// Owned bytes.
    Owned(Vec<u8>),
}

impl Body {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Body::Empty => &[],
            Body::Static(bytes) => bytes,
            Body::Owned(bytes) => bytes,
        }
    }

    /// The bytes as a buffer the kernel can send from, without a copy.
    pub(crate) fn into_cow(self) -> Cow<'static, [u8]> {
        match self {
            Body::Empty => Cow::Borrowed(&[]),
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

    pub(crate) fn body_len(&self) -> usize {
        self.body.as_bytes().len()
    }

    pub(crate) fn take_body(&mut self) -> Body {
        mem::take(&mut self.body)
    }

    /// Appends the status line and the header fields, up to and including the
    /// blank line, to `out`.
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
        if !self.bodiless() {
            out.extend_from_slice(b"Content-Length: ");
            push_number(out, self.body_len());
            out.extend_from_slice(b"\r\n");
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
