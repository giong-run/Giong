//! Turning the header fields of an HTTP/2 request into a [`Request`].
//!
//! A request keeps its head the way an HTTP/1.1 request does: as one buffer
//! of `name: value` lines with ranges into it. So the fields are written in
//! that form as they are decoded, behind a request line made from the
//! pseudo-header fields, and every accessor of [`Request`] works unchanged.
//!
//! The checks are RFC 9113's (section 8.2 and 8.3): a request that breaks
//! one is malformed, and its stream is reset with `PROTOCOL_ERROR`.

use std::net::SocketAddr;
use std::ops::Range;
use std::rc::Rc;

use crate::budget::Charge;
use crate::parse::{is_field_value, is_token_str, same_authority, Head};
use crate::request::{BodyBytes, Request};

/// Per-field overhead counted against the header list size (RFC 9113,
/// section 6.5.2).
const FIELD_OVERHEAD: usize = 32;

/// The largest field buffer a connection keeps between requests.
const KEEP_SCRATCH: usize = 4 * 1024;

/// What a header block makes of a request.
pub(crate) enum Built {
    Request(Request),
    /// Answer with this status without calling the handler: 431 for a
    /// header list over the limit, 417 for an `Expect` we cannot meet.
    Status(u16),
    /// Malformed: reset the stream with `PROTOCOL_ERROR`. The text says
    /// why, for tests and debuggers (a reset carries no message).
    Malformed(#[cfg_attr(not(test), allow(dead_code))] &'static str),
}

/// Collects the fields of one request header block.
pub(crate) struct Builder {
    /// The values of the pseudo-header fields, back to back, then (from
    /// `lines_from`) the `name: value\r\n` lines of the regular fields: the
    /// pseudo-header fields come first. A connection reuses this buffer
    /// from one request to the next.
    scratch: Vec<u8>,
    lines_from: usize,
    method: Option<Range<usize>>,
    scheme: Option<Range<usize>>,
    path: Option<Range<usize>>,
    authority: Option<Range<usize>>,
    host: Option<Range<usize>>,
    /// `cookie` fields, joined with `; ` (RFC 9113, section 8.2.3).
    cookies: Vec<u8>,
    regular: bool,
    content_length: Option<u64>,
    expect_continue: bool,
    /// Header list size so far, and the limit.
    size: usize,
    limit: usize,
    error: Option<&'static str>,
    status: Option<u16>,
    /// The fields are those of an HTTP/3 request: the same rules, another
    /// version on the request.
    h3: bool,
}

/// The fields HTTP/2 forbids because they are about one connection
/// (RFC 9113, section 8.2.2).
fn connection_specific(name: &[u8]) -> bool {
    matches!(
        name,
        b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding" | b"upgrade"
    )
}

/// A valid HTTP/2 field name: a token, in lowercase.
fn valid_name(name: &[u8]) -> bool {
    is_token_str(name) && !name.iter().any(u8::is_ascii_uppercase)
}

/// A valid field value: no control characters, and no whitespace at either
/// end (RFC 9113, section 8.2.1).
fn valid_value(value: &[u8]) -> bool {
    is_field_value(value)
        && !matches!(value.first(), Some(b' ' | b'\t'))
        && !matches!(value.last(), Some(b' ' | b'\t'))
}

fn parse_length(value: &[u8]) -> Option<u64> {
    if value.is_empty() || value.len() > 19 || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        value
            .iter()
            .fold(0u64, |acc, &b| acc * 10 + u64::from(b - b'0')),
    )
}

impl Builder {
    /// A builder for a header list of at most `limit` bytes (counted as
    /// RFC 9113 counts `SETTINGS_MAX_HEADER_LIST_SIZE`).
    #[cfg(test)]
    pub(crate) fn new(limit: usize) -> Self {
        Self::with_scratch(limit, Vec::new())
    }

    /// The same, writing into `scratch` (its contents are dropped), which
    /// [`into_scratch`](Self::into_scratch) gives back for the next request.
    pub(crate) fn with_scratch(limit: usize, mut scratch: Vec<u8>) -> Self {
        scratch.clear();
        Self {
            scratch,
            lines_from: 0,
            method: None,
            scheme: None,
            path: None,
            authority: None,
            host: None,
            cookies: Vec::new(),
            regular: false,
            content_length: None,
            expect_continue: false,
            size: 0,
            limit,
            error: None,
            status: None,
            h3: false,
        }
    }

    /// The same builder for the fields of an HTTP/3 request, which follow
    /// the rules of HTTP/2 (RFC 9114, sections 4.2 and 4.3).
    #[cfg(feature = "unstable-http3")]
    pub(crate) fn http3(mut self) -> Self {
        self.h3 = true;
        self
    }

    fn fail(&mut self, why: &'static str) {
        if self.error.is_none() {
            self.error = Some(why);
        }
    }

    fn set_pseudo(&mut self, which: u8, value: &[u8]) {
        let start = self.scratch.len();
        self.scratch.extend_from_slice(value);
        let range = Some(start..self.scratch.len());
        let slot = match which {
            b'm' => &mut self.method,
            b's' => &mut self.scheme,
            b'p' => &mut self.path,
            _ => &mut self.authority,
        };
        let twice = slot.is_some();
        *slot = range;
        if twice {
            self.fail("a pseudo-header field appears twice");
        }
    }

    /// Adds one decoded field.
    pub(crate) fn field(&mut self, name: &[u8], value: &[u8]) {
        self.size += name.len() + value.len() + FIELD_OVERHEAD;
        if self.size > self.limit {
            // Keep decoding (the table must follow the block), keep nothing.
            self.status = Some(431);
            return;
        }
        if self.error.is_some() || self.status.is_some() {
            return;
        }
        if let Some(pseudo) = name.strip_prefix(b":") {
            if self.regular {
                return self.fail("a pseudo-header field after a regular one");
            }
            let which = match pseudo {
                b"method" => {
                    if !is_token_str(value) || value.len() > 32 {
                        return self.fail("an invalid :method");
                    }
                    b'm'
                }
                b"scheme" => {
                    if value.is_empty() || !value.iter().all(|&b| (0x21..=0x7e).contains(&b)) {
                        return self.fail("an invalid :scheme");
                    }
                    b's'
                }
                b"path" => {
                    if value.is_empty() || !value.iter().all(|&b| (0x21..=0x7e).contains(&b)) {
                        return self.fail("an empty or invalid :path");
                    }
                    b'p'
                }
                b"authority" => {
                    // No user name or password (RFC 9113, section 8.3.1).
                    if !value.iter().all(|&b| (0x21..=0x7e).contains(&b)) || value.contains(&b'@') {
                        return self.fail("an invalid :authority");
                    }
                    b'a'
                }
                _ => return self.fail("an unknown or response pseudo-header field"),
            };
            return self.set_pseudo(which, value);
        }
        if !self.regular {
            self.regular = true;
            self.lines_from = self.scratch.len();
        }
        if !valid_name(name) {
            return self.fail("a field name that is not a lowercase token");
        }
        if !valid_value(value) {
            return self.fail("a field value with control characters or surrounding whitespace");
        }
        if connection_specific(name) {
            return self.fail("a connection-specific header field");
        }
        match name {
            b"te" if value != b"trailers" => {
                return self.fail("a TE header field other than \"trailers\"")
            }
            b"content-length" => match parse_length(value) {
                Some(n) if self.content_length.is_none_or_eq(n) => self.content_length = Some(n),
                _ => return self.fail("an invalid or conflicting content-length"),
            },
            b"expect" => {
                if value.eq_ignore_ascii_case(b"100-continue") {
                    self.expect_continue = true;
                } else {
                    self.status = Some(417);
                    return;
                }
            }
            b"host" => {
                if self.host.is_some() {
                    return self.fail("two host header fields");
                }
                let start = self.scratch.len() - self.lines_from + name.len() + 2;
                self.host = Some(start..start + value.len());
            }
            b"cookie" => {
                if !self.cookies.is_empty() {
                    self.cookies.extend_from_slice(b"; ");
                }
                self.cookies.extend_from_slice(value);
                return;
            }
            _ => {}
        }
        self.scratch.extend_from_slice(name);
        self.scratch.extend_from_slice(b": ");
        self.scratch.extend_from_slice(value);
        self.scratch.extend_from_slice(b"\r\n");
    }

    /// Checks the whole list and builds the request (with no body yet).
    pub(crate) fn finish(&mut self, peer: Option<SocketAddr>) -> Built {
        if let Some(status) = self.status {
            return Built::Status(status);
        }
        if let Some(why) = self.error {
            return Built::Malformed(why);
        }
        let Some(method) = self.method.clone() else {
            return Built::Malformed("no :method");
        };
        if !self.regular {
            self.lines_from = self.scratch.len();
        }
        let (pseudo, lines) = (&self.scratch[..], &self.scratch[self.lines_from..]);
        let connect = &pseudo[method.clone()] == b"CONNECT";
        let target = if connect {
            // RFC 9113, section 8.5: only :method and :authority.
            if self.scheme.is_some() || self.path.is_some() {
                return Built::Malformed("a CONNECT request with :scheme or :path");
            }
            match self.authority.clone() {
                Some(authority) if !authority.is_empty() => authority,
                _ => return Built::Malformed("a CONNECT request without :authority"),
            }
        } else {
            if self.scheme.is_none() {
                return Built::Malformed("no :scheme");
            }
            let Some(path) = self.path.clone() else {
                return Built::Malformed("no :path");
            };
            let value = &pseudo[path.clone()];
            let options = &pseudo[method.clone()] == b"OPTIONS";
            if !(value.starts_with(b"/") || (options && value == b"*")) {
                return Built::Malformed("a :path that is neither absolute nor *");
            }
            path
        };
        // RFC 9113, section 8.3.1. HTTP/1.1 holds an absolute target's
        // authority to the same rule (see `parse_head`).
        if let (Some(authority), Some(host)) = (&self.authority, &self.host) {
            if !same_authority(&lines[host.clone()], &pseudo[authority.clone()]) {
                return Built::Malformed("a host header field that differs from :authority");
            }
        }

        // The head, as HTTP/1.1 would have it.
        let mut buf = Vec::with_capacity(
            method.len() + target.len() + 11 + lines.len() + self.cookies.len() + 64,
        );
        buf.extend_from_slice(&pseudo[method.clone()]);
        buf.push(b' ');
        let target_start = buf.len();
        buf.extend_from_slice(&pseudo[target.clone()]);
        let target_end = buf.len();
        buf.extend_from_slice(if self.h3 {
            b" HTTP/3\r\n"
        } else {
            b" HTTP/2\r\n"
        });
        let headers_start = buf.len();
        if self.host.is_none() {
            if let Some(authority) = self.authority.clone().filter(|a| !a.is_empty()) {
                buf.extend_from_slice(b"host: ");
                buf.extend_from_slice(&pseudo[authority]);
                buf.extend_from_slice(b"\r\n");
            }
        }
        buf.extend_from_slice(lines);
        if !self.cookies.is_empty() {
            buf.extend_from_slice(b"cookie: ");
            buf.extend_from_slice(&self.cookies);
            buf.extend_from_slice(b"\r\n");
        }
        let headers_end = buf.len();
        buf.extend_from_slice(b"\r\n");
        let len = buf.len();
        let head = Head {
            method: 0..method.len(),
            target: target_start..target_end,
            minor: 1,
            headers: headers_start..headers_end,
            len,
            content_length: self.content_length,
            chunked: false,
            close: false,
            keep_alive: true,
            expect_continue: self.expect_continue,
            h2: true,
            h3: self.h3,
        };
        Built::Request(Request {
            buf: Rc::new(buf),
            start: 0,
            head,
            body: BodyBytes::InBuffer(len..len),
            peer,
            charge: Charge::none(),
        })
    }

    /// The buffer the fields were written to, emptied, for the next request.
    /// One that grew past `KEEP_SCRATCH` (a request with large headers) is
    /// dropped instead of kept for the life of the connection.
    pub(crate) fn into_scratch(mut self) -> Vec<u8> {
        if self.scratch.capacity() > KEEP_SCRATCH {
            return Vec::new();
        }
        self.scratch.clear();
        self.scratch
    }
}

trait NoneOrEq {
    fn is_none_or_eq(&self, n: u64) -> bool;
}

impl NoneOrEq for Option<u64> {
    fn is_none_or_eq(&self, n: u64) -> bool {
        self.map_or(true, |prev| prev == n)
    }
}

/// Checks the fields of a trailer block: no pseudo-header fields, and the
/// same rules as other fields. The values themselves are dropped: a
/// [`Request`] has no trailers.
pub(crate) struct Trailers {
    pub(crate) error: Option<&'static str>,
}

impl Trailers {
    pub(crate) fn new() -> Self {
        Self { error: None }
    }

    pub(crate) fn field(&mut self, name: &[u8], value: &[u8]) {
        if self.error.is_some() {
            return;
        }
        if name.starts_with(b":") {
            self.error = Some("a pseudo-header field in trailers");
        } else if !valid_name(name) || !valid_value(value) || connection_specific(name) {
            self.error = Some("an invalid field in trailers");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(fields: &[(&str, &str)]) -> Built {
        let mut b = Builder::new(16 * 1024);
        for (n, v) in fields {
            b.field(n.as_bytes(), v.as_bytes());
        }
        b.finish(None)
    }

    fn request(fields: &[(&str, &str)]) -> Request {
        match build(fields) {
            Built::Request(req) => req,
            Built::Status(s) => panic!("status {s}"),
            Built::Malformed(why) => panic!("malformed: {why}"),
        }
    }

    const BASE: [(&str, &str); 4] = [
        (":method", "GET"),
        (":scheme", "https"),
        (":path", "/a/b?x=1"),
        (":authority", "example.com"),
    ];

    #[test]
    fn a_request_reads_like_an_http_1_1_one() {
        let mut fields = BASE.to_vec();
        fields.extend([
            ("accept", "*/*"),
            ("cookie", "a=1"),
            ("x-id", "7"),
            ("cookie", "b=2"),
            ("content-length", "0"),
        ]);
        let req = request(&fields);
        assert_eq!(req.method(), "GET");
        assert_eq!(req.target(), "/a/b?x=1");
        assert_eq!(req.path(), "/a/b");
        assert_eq!(req.query(), Some("x=1"));
        assert_eq!(req.header_str("host"), Some("example.com"));
        assert_eq!(req.header_str("cookie"), Some("a=1; b=2"));
        assert_eq!(req.header_str("x-id"), Some("7"));
        assert_eq!(req.version(), crate::Version::Http2);
        assert!(req.body().is_empty());
        let names: Vec<_> = req.headers().map(|(n, _)| n.to_string()).collect();
        assert_eq!(
            names,
            ["host", "accept", "x-id", "content-length", "cookie"]
        );
    }

    #[test]
    fn connect_and_options_have_their_own_targets() {
        let req = request(&[(":method", "CONNECT"), (":authority", "db:5432")]);
        assert_eq!(req.target(), "db:5432");
        let req = request(&[(":method", "OPTIONS"), (":scheme", "http"), (":path", "*")]);
        assert_eq!(req.target(), "*");
    }

    #[test]
    fn malformed_requests_are_refused() {
        let cases: Vec<Vec<(&str, &str)>> = vec![
            vec![(":scheme", "https"), (":path", "/")],
            vec![(":method", "GET"), (":path", "/")],
            vec![(":method", "GET"), (":scheme", "https")],
            vec![(":method", "GET"), (":scheme", "https"), (":path", "")],
            vec![(":method", "GET"), (":scheme", "https"), (":path", "a")],
            vec![
                (":method", "GET"),
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                (":status", "200"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                (":foo", "x"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                ("a", "b"),
                (":path", "/"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                ("Upper", "x"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                ("connection", "close"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                ("te", "gzip"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                ("a", " padded"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                ("a", "x\0y"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                ("content-length", "1"),
                ("content-length", "2"),
            ],
            vec![
                (":method", "GET"),
                (":scheme", "https"),
                (":path", "/"),
                (":authority", "a"),
                ("host", "b"),
            ],
            vec![
                (":method", "CONNECT"),
                (":authority", "a:1"),
                (":path", "/"),
            ],
        ];
        for fields in cases {
            assert!(
                matches!(build(&fields), Built::Malformed(_)),
                "{fields:?} was accepted"
            );
        }
        let mut te = BASE.to_vec();
        te.push(("te", "trailers"));
        request(&te);
    }

    #[test]
    fn host_and_authority_are_compared_as_http_1_1_compares_them() {
        // Host names are case-insensitive: the same host in another case
        // is no disagreement (and HTTP/1.1 compares an absolute target's
        // authority with Host the same way).
        let mut fields = BASE.to_vec();
        fields.push(("host", "Example.COM"));
        assert_eq!(request(&fields).header_str("host"), Some("Example.COM"));
        // RFC 9113, section 8.3.1: no user name or password in :authority.
        for authority in ["user@example.com", "u:p@example.com"] {
            let mut fields = BASE.to_vec();
            fields[3] = (":authority", authority);
            assert!(
                matches!(build(&fields), Built::Malformed(_)),
                "{authority} was accepted"
            );
        }
    }

    #[test]
    fn oversized_lists_and_unknown_expectations_get_a_status() {
        let mut b = Builder::new(200);
        for (n, v) in BASE {
            b.field(n.as_bytes(), v.as_bytes());
        }
        b.field(b"x-big", &[b'a'; 200]);
        assert!(matches!(b.finish(None), Built::Status(431)));
        let mut fields = BASE.to_vec();
        fields.push(("expect", "something"));
        assert!(matches!(build(&fields), Built::Status(417)));
        fields.pop();
        fields.push(("expect", "100-continue"));
        assert!(request(&fields).head.expect_continue);
    }
}
