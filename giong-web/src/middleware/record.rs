//! [`Record`]: one JSON line per request with the request and response in
//! full, for turning real requests into tests.

use std::rc::Rc;
use std::time::{Instant, SystemTime};

use crate::json::{escape_into, JsonWriter};
use crate::log::{timestamp, worker, Sink};
use crate::{Error, Middleware, Next, Req, Response};

/// Body bytes kept per side by default.
const DEFAULT_MAX_BODY: usize = 64 * 1024;
/// The most [`Record::max_body`] accepts: two bodies of this size, escaped,
/// still fit the log writer's 1 MiB per worker.
const MAX_MAX_BODY: usize = 128 * 1024;
/// Header values replaced by `[redacted]` unless the record keeps them.
const SECRET_HEADERS: [&str; 6] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "x-auth-token",
];
/// What a redacted value is replaced with.
const REDACTED: &str = "[redacted]";

/// Writes one JSON line per answered request with the request and the
/// response, headers and bodies included: an `exchange`. `giong test record`
/// turns chosen exchanges into tests, and `giong dev` shows them.
///
/// It is meant for development: add it only when you want recordings, for
/// example when `GIONG_RECORD` is set. Code that does not add it pays
/// nothing.
///
/// ```no_run
/// use giong_web::middleware::{Logger, Record, RequestId};
/// use giong_web::{Router, Sink};
///
/// # async fn handler(_: giong_web::Req) -> Result<giong_web::Response, giong_web::Error> { todo!() }
/// let mut router = Router::new()
///     .get("/", handler)
///     .layer(RequestId::new())
///     .layer(Logger::new(Sink::Stdout));
/// if std::env::var_os("GIONG_RECORD").is_some() {
///     router = router.layer(Record::new(Sink::Stdout));
/// }
/// let app = router.build(());
/// ```
///
/// A line looks like this (on one line):
///
/// ```json
/// {"ts":"2026-11-22T08:40:01.123456Z","level":"info","event":"exchange","method":"POST",
///  "path":"/users","query":null,"route":"/users","status":201,"duration_us":153,
///  "request_id":"5f3a9c2e8b41d706000100000000002a","worker":0,
///  "request":{"headers":[["content-type","application/json"],["authorization","[redacted]"]],
///             "body":"{\"name\":\"ada\"}","body_len":14,"truncated":false,"streamed":false},
///  "response":{"headers":[["content-type","application/json"]],
///              "body":"{\"id\":7}","body_len":8,"truncated":false,"streamed":false}}
/// ```
///
/// | Field | Value |
/// |---|---|
/// | `method`, `path`, `query`, `route`, `status`, `request_id`, `worker`, `ts` | as in [`Logger`](super::Logger)'s line, except that the path and the query are never cut and the query is always there (`null` when the request had none) |
/// | `duration_us` | microseconds until the response was ready |
/// | `request`, `response` | `headers`: every header as `[name, value]` in order, names in lower case, secrets redacted (below); `body` (UTF-8 text) or `body_base64` (anything else), at most [`max_body`](Self::max_body) bytes; `body_len`: the whole body's length; `truncated`: whether bytes were left out; `streamed`: the body was read or written as a stream and is not recorded |
///
/// The values of `Authorization`, `Proxy-Authorization`, `Cookie`,
/// `Set-Cookie`, `X-Api-Key` and `X-Auth-Token`, and of any header named
/// with [`redact_header`](Self::redact_header), are written as
/// `[redacted]`. Bodies and query strings are written as they are: do not
/// record traffic with real personal data.
///
/// A request dropped before its response was ready (a client gone, a
/// [`Timeout`](super::Timeout) further out) writes no line. Lines go
/// through the background writer described at [`Sink`]; a line too long
/// for it is dropped and counted, like any other.
///
/// Put it after [`RequestId`](super::RequestId), so that lines carry the
/// id, and after [`Compression`](super::Compression)'s position if you
/// want the body as the handler wrote it (a `Record` layered before
/// `Compression` sees the gzipped body, in `body_base64`).
#[derive(Clone, Debug)]
pub struct Record {
    sink: Sink,
    max_body: usize,
    /// Lower-case header names whose values are redacted, besides the
    /// built-in ones.
    redact: Rc<[Box<str>]>,
}

impl Record {
    /// A recorder writing to `sink`, keeping up to 64 KiB of each body.
    pub fn new(sink: Sink) -> Self {
        Self {
            sink,
            max_body: DEFAULT_MAX_BODY,
            redact: Rc::from(Vec::new()),
        }
    }

    /// Keeps up to `bytes` of each body (default 64 KiB; the rest is left
    /// out and `truncated` is `true`). `0` records no bodies.
    ///
    /// # Panics
    ///
    /// Panics with `GIONG-E1213` above 128 KiB: a longer line would not fit
    /// the log writer's buffer.
    #[track_caller]
    pub fn max_body(mut self, bytes: usize) -> Self {
        if bytes > MAX_MAX_BODY {
            super::invalid_config(
                format!("Record::max_body({bytes}) is above the 131072 bytes a record can hold"),
                "pass 131072 or less; record larger bodies in the handler itself.",
            );
        }
        self.max_body = bytes;
        self
    }

    /// Also writes the value of the header `name` as `[redacted]` (case
    /// does not matter).
    pub fn redact_header(mut self, name: &str) -> Self {
        let mut names: Vec<Box<str>> = self.redact.iter().cloned().collect();
        names.push(name.to_ascii_lowercase().into());
        self.redact = names.into();
        self
    }

    fn secret(&self, name: &str) -> bool {
        SECRET_HEADERS.iter().any(|s| name.eq_ignore_ascii_case(s))
            || self.redact.iter().any(|s| name.eq_ignore_ascii_case(s))
    }

    /// `"headers":[...]` for any header list.
    fn headers<'h>(&self, out: &mut String, headers: impl Iterator<Item = (&'h str, &'h [u8])>) {
        out.push_str("\"headers\":[");
        for (i, (name, value)) in headers.enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push('[');
            escape_into(out, &name.to_ascii_lowercase());
            out.push(',');
            if self.secret(name) {
                escape_into(out, REDACTED);
            } else {
                escape_into(out, &String::from_utf8_lossy(value));
            }
            out.push(']');
        }
        out.push(']');
    }

    /// The rest of a side's object after its headers.
    fn body(&self, out: &mut String, body: &[u8], streamed: bool) {
        if streamed {
            out.push_str(",\"body_len\":null,\"truncated\":false,\"streamed\":true}");
            return;
        }
        let kept = &body[..body.len().min(self.max_body)];
        match text(kept) {
            Some(t) => {
                out.push_str(",\"body\":");
                escape_into(out, t);
            }
            None => {
                out.push_str(",\"body_base64\":\"");
                base64_into(out, kept);
                out.push('"');
            }
        }
        out.push_str(&format!(
            ",\"body_len\":{},\"truncated\":{},\"streamed\":false}}",
            body.len(),
            kept.len() < body.len()
        ));
    }
}

impl<S: 'static> Middleware<S> for Record {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        let start = Instant::now();
        let ts = SystemTime::now();
        let http = req.http();
        let mut request = String::with_capacity(256 + http.body().len().min(self.max_body));
        request.push('{');
        self.headers(&mut request, http.headers());
        self.body(&mut request, http.body(), http.body_stream().is_some());
        let method = req.method().to_owned();
        let path = req.path().to_owned();
        let query = req.query_string().map(str::to_owned);
        let route = req.route().map(str::to_owned);
        let id = req.request_id().map(str::to_owned);

        let resp = next.run(req).await;
        let elapsed = start.elapsed();

        let mut response = String::with_capacity(256 + resp.body_len().min(self.max_body));
        response.push('{');
        self.headers(
            &mut response,
            resp.headers().map(|(n, v)| (n, v.as_bytes())),
        );
        self.body(
            &mut response,
            resp.body_bytes(),
            resp.is_streaming() || resp.body_is_file(),
        );

        let mut line = JsonWriter::with_capacity(128 + request.len() + response.len());
        line.str("ts", &timestamp(ts));
        line.str("level", "info");
        line.str("event", "exchange");
        line.str("method", &method);
        line.str("path", &path);
        match &query {
            Some(q) => line.str("query", q),
            None => line.null("query"),
        }
        match &route {
            Some(r) => line.str("route", r),
            None => line.null("route"),
        }
        line.u64("status", u64::from(resp.status()));
        line.u64("duration_us", elapsed.as_micros() as u64);
        match &id {
            Some(i) => line.str("request_id", i),
            None => line.null("request_id"),
        }
        if let Some(n) = worker() {
            line.u64("worker", n as u64);
        }
        line.raw("request", &request);
        line.raw("response", &response);
        self.sink.write(&line.finish_line());
        Ok(resp)
    }
}

/// `body` as text when it is UTF-8 without control characters other than
/// tab, line feed and carriage return; `None` when it should be base64.
fn text(body: &[u8]) -> Option<&str> {
    let s = std::str::from_utf8(body).ok()?;
    s.chars()
        .all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .then_some(s)
}

/// Standard base64 (RFC 4648, with padding).
fn base64_into(out: &mut String, input: &[u8]) {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_rfc_4648_examples() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            let mut out = String::new();
            base64_into(&mut out, input.as_bytes());
            assert_eq!(out, expected, "{input:?}");
        }
    }

    #[test]
    fn text_refuses_control_characters_and_invalid_utf8() {
        assert_eq!(text(b"hello\n\tworld\r"), Some("hello\n\tworld\r"));
        assert_eq!(text("héllo".as_bytes()), Some("héllo"));
        assert_eq!(text(b"a\0b"), None);
        assert_eq!(text(b"\x1b[31m"), None);
        assert_eq!(text(&[0xff, 0xfe]), None);
        assert_eq!(text(b""), Some(""));
    }

    #[test]
    fn a_body_cut_inside_a_character_falls_back_to_base64() {
        let rec = Record::new(Sink::Stdout).max_body(2);
        let mut out = String::new();
        rec.body(&mut out, "é!".as_bytes(), false);
        // "é" is two bytes: kept whole, as text.
        assert!(out.contains(r#""body":"é""#), "{out}");
        let mut out = String::new();
        rec.body(&mut out, "aé".as_bytes(), false);
        // "a" plus half of "é" is not UTF-8.
        assert!(out.contains(r#""body_base64":"YcM=""#), "{out}");
        assert!(out.contains(r#""truncated":true"#), "{out}");
    }
}
