//! [`Record`]: one JSON line per request with the request and response in
//! full, for turning real requests into tests.

use std::borrow::Cow;
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
/// Words that make a query parameter, form field or JSON key a secret
/// wherever they appear in its name (see [`secret_name`]).
const SECRET_WORDS: [&str; 12] = [
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "token",
    "apikey",
    "cvv",
    "cvc",
    "ssn",
    "authorization",
    "card",
];
/// Words that make a following `key` a secret: `api_key`, `privateKey`.
const KEY_KINDS: [&str; 5] = ["api", "private", "secret", "access", "signing"];

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
/// | `method`, `path`, `query`, `route`, `status`, `request_id`, `worker`, `ts` | as in [`Logger`](super::Logger)'s line, except that the path and the query are never cut, the query is always there (`null` when the request had none) and its secrets are redacted (below) |
/// | `duration_us` | microseconds until the response was ready |
/// | `request`, `response` | `headers`: every header as `[name, value]` in order, names in lower case, secrets redacted (below); `body` (UTF-8 text) or `body_base64` (anything else), secrets redacted (below), at most [`max_body`](Self::max_body) bytes; `body_len`: the length of the whole body as it was sent; `truncated`: whether bytes were left out; `streamed`: the body was read or written as a stream and is not recorded |
///
/// The values of `Authorization`, `Proxy-Authorization`, `Cookie`,
/// `Set-Cookie`, `X-Api-Key` and `X-Auth-Token`, of any header named with
/// [`redact_header`](Self::redact_header), and of any other header whose
/// name says secret (below: `X-Session-Token`, `Private-Token`,
/// `X-Goog-Api-Key`) are written as `[redacted]`. So are the values of
/// query parameters, form fields (`application/x-www-form-urlencoded` and
/// `multipart/form-data` bodies) and JSON keys at any depth (bodies of a
/// JSON content type, or that start with `{` or `[`) whose name says
/// secret, on both sides:
///
/// - a word of the name is `password`, `passwd`, `pwd`, `passphrase`,
///   `secret`, `token`, `apikey`, `cvv`, `cvc`, `ssn`, `authorization` or
///   `card` (words are split at anything not a letter or digit and at
///   camelCase: `newPassword`, `card_number`, `access_token`), or
/// - `key` follows `api`, `private`, `secret`, `access` or `signing`
///   (`api_key`, `privateKey`; `key` alone is not a secret).
///
/// A JSON value is replaced whole, objects included: `"card":"[redacted]"`.
/// A form or JSON body that is not valid UTF-8 is read with each bad byte as
/// U+FFFD; when it holds a secret, the record keeps that redacted text
/// instead of the raw bytes.
/// [`redact_field`](Self::redact_field) adds a name and
/// [`keep_field`](Self::keep_field) keeps one (a header too, except the six
/// above). Other text is written as it is: do not record traffic with real
/// personal data.
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
    /// Field names redacted besides those [`secret_name`] picks.
    redact_fields: Rc<[Box<str>]>,
    /// Field names [`secret_name`] picks that are kept.
    keep_fields: Rc<[Box<str>]>,
}

impl Record {
    /// A recorder writing to `sink`, keeping up to 64 KiB of each body.
    pub fn new(sink: Sink) -> Self {
        Self {
            sink,
            max_body: DEFAULT_MAX_BODY,
            redact: Rc::from(Vec::new()),
            redact_fields: Rc::from(Vec::new()),
            keep_fields: Rc::from(Vec::new()),
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
        self.redact = with(&self.redact, name);
        self
    }

    /// Also writes the value of the query parameter, form field or JSON key
    /// `name` as `[redacted]` (case does not matter), besides the names that
    /// say secret.
    pub fn redact_field(mut self, name: &str) -> Self {
        self.redact_fields = with(&self.redact_fields, name);
        self
    }

    /// Writes the query parameter, form field, JSON key or header `name` as
    /// it is, although its name says secret (case does not matter): a
    /// `token` that is a page cursor, say. The six headers that are always
    /// redacted stay redacted.
    pub fn keep_field(mut self, name: &str) -> Self {
        self.keep_fields = with(&self.keep_fields, name);
        self
    }

    fn secret_field(&self, name: &str) -> bool {
        if self
            .keep_fields
            .iter()
            .any(|k| name.eq_ignore_ascii_case(k))
        {
            return false;
        }
        self.redact_fields
            .iter()
            .any(|k| name.eq_ignore_ascii_case(k))
            || secret_name(name)
    }

    /// A query string or form body with the values of secret names
    /// replaced: `user=ada&password=[redacted]`.
    fn redact_pairs<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let mut out = String::new();
        let mut changed = false;
        for (i, pair) in s.split('&').enumerate() {
            if i > 0 {
                out.push('&');
            }
            match pair.split_once('=') {
                Some((name, value))
                    if !value.is_empty() && self.secret_field(&percent_decode(name)) =>
                {
                    out.push_str(name);
                    out.push('=');
                    out.push_str(REDACTED);
                    changed = true;
                }
                _ => out.push_str(pair),
            }
        }
        if changed {
            Cow::Owned(out)
        } else {
            Cow::Borrowed(s)
        }
    }

    /// JSON text with the value of every key that is a secret, at any
    /// depth, replaced by `"[redacted]"`, whatever the value (a nested
    /// object goes whole). It does not need valid JSON: a body cut short
    /// or malformed is redacted as far as keys can be told apart, and a
    /// secret's value that does not end is redacted to the end.
    fn redact_json<'a>(&self, s: &'a str) -> Cow<'a, str> {
        let b = s.as_bytes();
        let mut out = String::new();
        let mut copied = 0;
        let mut i = 0;
        while i < b.len() {
            if b[i] != b'"' {
                i += 1;
                continue;
            }
            let end = string_end(b, i);
            let colon = skip_space(b, end);
            if colon < b.len() && b[colon] == b':' {
                let key = unescape(&s[i + 1..end - 1]);
                if self.secret_field(&key) {
                    let start = skip_space(b, colon + 1);
                    let stop = value_end(b, start);
                    if stop > start {
                        out.push_str(&s[copied..start]);
                        out.push('"');
                        out.push_str(REDACTED);
                        out.push('"');
                        copied = stop;
                        i = stop;
                        continue;
                    }
                }
            }
            i = end;
        }
        if copied == 0 {
            return Cow::Borrowed(s);
        }
        out.push_str(&s[copied..]);
        Cow::Owned(out)
    }

    /// A body with its secrets redacted: form fields when `content_type`
    /// says form, JSON keys when it says JSON or the body looks like JSON.
    fn redact_body<'a>(&self, body: &'a [u8], content_type: Option<&[u8]>) -> Cow<'a, [u8]> {
        let ct = content_type.map(|c| String::from_utf8_lossy(c).to_ascii_lowercase());
        // A body that is not UTF-8 is scanned through a lossy view: when it
        // holds a secret, the record keeps the redacted text (invalid bytes
        // become U+FFFD) rather than the raw bytes as base64, secret included.
        let text = String::from_utf8_lossy(body);
        let ct = ct.as_deref().unwrap_or("");
        let redacted = if ct.contains("x-www-form-urlencoded") {
            self.redact_pairs(&text)
        } else if ct.starts_with("multipart/form-data") {
            // The boundary as sent: it is case-sensitive.
            let original = String::from_utf8_lossy(content_type.unwrap_or_default());
            match boundary(&original) {
                Some(b) => self.redact_multipart(&text, b),
                None => return Cow::Borrowed(body),
            }
        } else if ct.contains("json") || text.trim_start().starts_with(['{', '[']) {
            self.redact_json(&text)
        } else {
            return Cow::Borrowed(body);
        };
        match redacted {
            Cow::Borrowed(_) => Cow::Borrowed(body),
            Cow::Owned(t) => Cow::Owned(t.into_bytes()),
        }
    }

    /// Whether a header's value is redacted: one of [`SECRET_HEADERS`], one
    /// named with [`redact_header`](Self::redact_header), or a name that
    /// says secret by the rule fields follow (`x-session-token`,
    /// `private-token`), unless kept with [`keep_field`](Self::keep_field).
    fn secret(&self, name: &str) -> bool {
        SECRET_HEADERS.iter().any(|s| name.eq_ignore_ascii_case(s))
            || self.redact.iter().any(|s| name.eq_ignore_ascii_case(s))
            || self.secret_field(name)
    }

    /// A `multipart/form-data` body with the content of each part whose
    /// field name says secret replaced: as far as parts can be told apart
    /// when the body is cut short, and to the end for a part that does not
    /// end.
    fn redact_multipart<'a>(&self, s: &'a str, boundary: &str) -> Cow<'a, str> {
        let delimiter = format!("--{boundary}");
        let next = format!("\r\n{delimiter}");
        let mut out = String::new();
        let mut copied = 0;
        let Some(mut at) = s.find(&delimiter) else {
            return Cow::Borrowed(s);
        };
        loop {
            let part = at + delimiter.len();
            if s[part..].starts_with("--") {
                break;
            }
            let Some(head_len) = s[part..].find("\r\n\r\n") else {
                break;
            };
            let content = part + head_len + 4;
            let end = s[content..].find(&next).map(|e| content + e);
            let stop = end.unwrap_or(s.len());
            if stop > content
                && part_name(&s[part..part + head_len]).is_some_and(|n| self.secret_field(&n))
            {
                out.push_str(&s[copied..content]);
                out.push_str(REDACTED);
                copied = stop;
            }
            match end {
                Some(e) => at = e + 2,
                None => break,
            }
        }
        if copied == 0 {
            return Cow::Borrowed(s);
        }
        out.push_str(&s[copied..]);
        Cow::Owned(out)
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

    /// The rest of a side's object after its headers: `body` is the body
    /// with its secrets redacted, `len` the length of the body as it was.
    fn body(&self, out: &mut String, body: &[u8], len: usize, streamed: bool) {
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
            ",\"body_len\":{len},\"truncated\":{},\"streamed\":false}}",
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
        let body = self.redact_body(http.body(), req.header("content-type"));
        self.body(
            &mut request,
            &body,
            http.body().len(),
            http.body_stream().is_some(),
        );
        let method = req.method().to_owned();
        let path = req.path().to_owned();
        let query = req
            .query_string()
            .map(|q| self.redact_pairs(q).into_owned());
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
        let content_type = resp
            .headers()
            .find(|(n, _)| n.eq_ignore_ascii_case("content-type"))
            .map(|(_, v)| v.as_bytes());
        let body = self.redact_body(resp.body_bytes(), content_type);
        self.body(
            &mut response,
            &body,
            resp.body_bytes().len(),
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

/// `names` with `name` added, in lower case.
fn with(names: &[Box<str>], name: &str) -> Rc<[Box<str>]> {
    let mut v: Vec<Box<str>> = names.to_vec();
    v.push(name.to_ascii_lowercase().into());
    v.into()
}

/// Whether a field name says secret. The name is split into lower-case
/// words at anything not a letter or digit and where camelCase starts a
/// word (`newPassword`: `new`, `password`). It is a secret when a word is
/// one of [`SECRET_WORDS`], or when a word `key` follows one of
/// [`KEY_KINDS`] (`api_key`, `privateKey`, also written as one word:
/// `secretkey`). `key` alone, `sort_key` and `cardinality` are not: a
/// plain `key` is as often a name the service made that a later request
/// reuses. `giong record save` applies the same rule.
fn secret_name(name: &str) -> bool {
    let mut words: Vec<String> = Vec::new();
    let mut word = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if !c.is_alphanumeric() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !word.is_empty() {
            words.push(std::mem::take(&mut word));
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        word.extend(c.to_lowercase());
    }
    if !word.is_empty() {
        words.push(word);
    }
    words.iter().enumerate().any(|(i, w)| {
        SECRET_WORDS.contains(&w.as_str())
            || (w == "key" && i > 0 && KEY_KINDS.contains(&words[i - 1].as_str()))
            || KEY_KINDS.iter().any(|k| w.strip_prefix(k) == Some("key"))
    })
}

/// The `boundary` parameter of a `multipart/form-data` content type, its
/// quotes taken off.
fn boundary(content_type: &str) -> Option<&str> {
    content_type
        .split(';')
        .skip(1)
        .find_map(|param| {
            let (k, v) = param.split_once('=')?;
            k.trim().eq_ignore_ascii_case("boundary").then(|| {
                let v = v.trim();
                v.strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(v)
            })
        })
        .filter(|b| !b.is_empty())
}

/// The field name of a multipart part, from the `name` parameter of its
/// `Content-Disposition` header (`filename` is not it).
fn part_name(head: &str) -> Option<String> {
    let value = head.split("\r\n").find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-disposition")
            .then_some(v)
    })?;
    value.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        k.trim().eq_ignore_ascii_case("name").then(|| {
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(v);
            percent_decode(v).into_owned()
        })
    })
}

/// A query or form name with `%XX` and `+` decoded (lossily).
fn percent_decode(s: &str) -> Cow<'_, str> {
    if !s.contains(['%', '+']) {
        return Cow::Borrowed(s);
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => {
                match std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .filter(|h| h.bytes().all(|c| c.is_ascii_hexdigit()))
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// The index just past the JSON string that starts with the quote at
/// `start`, or the end of the input when it does not end.
fn string_end(b: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    b.len()
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// The index just past the JSON value at `start`: a string, an object or
/// array with everything inside it, or a number or literal. A value that
/// does not end runs to the end of the input.
fn value_end(b: &[u8], start: usize) -> usize {
    match b.get(start) {
        None => start,
        Some(b'"') => string_end(b, start),
        Some(b'{' | b'[') => {
            let mut depth = 0usize;
            let mut i = start;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = string_end(b, i);
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            b.len()
        }
        Some(_) => {
            let mut i = start;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
            {
                i += 1;
            }
            i
        }
    }
}

/// A JSON key's text with its escapes resolved, for matching names.
fn unescape(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\\') {
        return Cow::Borrowed(raw);
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('u') => {
                let hex: String = chars.by_ref().take(4).collect();
                // Four hex digits: from_str_radix alone also takes "+041".
                let digits = hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit());
                if let Some(ch) = digits
                    .then(|| u32::from_str_radix(&hex, 16).ok())
                    .flatten()
                    .and_then(char::from_u32)
                {
                    out.push(ch);
                }
            }
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    Cow::Owned(out)
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
    fn query_names_decode_two_hex_digits_only() {
        assert_eq!(percent_decode("api%5Fkey"), "api_key");
        assert_eq!(percent_decode("a+b"), "a b");
        // A sign is not a hex digit (u8::from_str_radix takes "+5").
        assert_eq!(percent_decode("%+5"), "% 5");
        assert_eq!(percent_decode("%4"), "%4");
    }

    #[test]
    fn json_keys_unescape_four_hex_digits_only() {
        assert_eq!(unescape(r"\u0070assword"), "password");
        assert_eq!(unescape(r"a\tb"), "a\tb");
        // A sign is not a hex digit (u32::from_str_radix takes "+041").
        assert_eq!(unescape(r"\u+041x"), "x");
        assert_eq!(unescape(r"\u41"), "");
    }

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
    fn secret_names_by_word() {
        for name in [
            "password",
            "Password",
            "newPassword",
            "new_password",
            "passwd",
            "pwd",
            "passphrase",
            "client_secret",
            "clientSecret",
            "token",
            "access_token",
            "refreshToken",
            "id-token",
            "api_key",
            "apiKey",
            "apikey",
            "x-api-key",
            "API_KEY",
            "privateKey",
            "secret_key",
            "secretkey",
            "signing.key",
            "accessKey",
            "card",
            "card_number",
            "cardNumber",
            "cvv",
            "cvc",
            "ssn",
            "authorization",
        ] {
            assert!(secret_name(name), "{name} should be a secret");
        }
        for name in [
            "sort_key",
            "key",
            "KEY",
            "keys",
            "keyword",
            "monkey",
            "cardinality",
            "discard",
            "tokens_used",
            "user",
            "name",
            "email",
            "page",
            "",
        ] {
            assert!(!secret_name(name), "{name} should not be a secret");
        }
    }

    #[test]
    fn query_and_form_values_of_secrets_are_redacted() {
        let rec = Record::new(Sink::Stdout);
        assert_eq!(rec.redact_pairs("x=1&y=two"), "x=1&y=two");
        assert!(matches!(rec.redact_pairs("x=1&y=two"), Cow::Borrowed(_)));
        assert_eq!(
            rec.redact_pairs("user=ada&password=hunter2&api_key=AK-1&next=%2F"),
            "user=ada&password=[redacted]&api_key=[redacted]&next=%2F"
        );
        // Encoded names, empty values, names without a value, repeats.
        assert_eq!(
            rec.redact_pairs("api%5Fkey=a&token=&token&token=b&&=c"),
            "api%5Fkey=[redacted]&token=&token&token=[redacted]&&=c"
        );
        let rec = rec.keep_field("token").redact_field("Session");
        assert_eq!(
            rec.redact_pairs("token=cursor&session=s&password=p"),
            "token=cursor&session=[redacted]&password=[redacted]"
        );
    }

    #[test]
    fn json_values_of_secrets_are_redacted_at_any_depth() {
        let rec = Record::new(Sink::Stdout);
        let same = r#"{"name":"ada","tags":["password","token"],"n":1}"#;
        assert!(matches!(rec.redact_json(same), Cow::Borrowed(_)));
        assert_eq!(
            rec.redact_json(
                r#"{"user":"ada","password":"hunter2","card":{"number":"4242424242424242","cvv":123},"items":[{"apiKey":"AK"},{"token":null}],"pin":7}"#
            ),
            r#"{"user":"ada","password":"[redacted]","card":"[redacted]","items":[{"apiKey":"[redacted]"},{"token":"[redacted]"}],"pin":7}"#
        );
        // Spacing kept, escapes in keys and values, numbers and literals.
        assert_eq!(
            rec.redact_json("{ \"pass\\u0077ord\" : \"a\\\"b\" ,\n \"cvv\": 123 , \"ok\": true }"),
            "{ \"pass\\u0077ord\" : \"[redacted]\" ,\n \"cvv\": \"[redacted]\" , \"ok\": true }"
        );
        // A quote inside a value does not start a key.
        assert_eq!(
            rec.redact_json(r#"{"note":"say \"password\": no","secret":"s"}"#),
            r#"{"note":"say \"password\": no","secret":"[redacted]"}"#
        );
        // Cut short or malformed: redacted as far as it goes, to the end.
        assert_eq!(
            rec.redact_json(r#"{"a":1,"password":"hunt"#),
            r#"{"a":1,"password":"[redacted]""#
        );
        assert_eq!(
            rec.redact_json(r#"{"card":{"number":"4242"#),
            r#"{"card":"[redacted]""#
        );
        assert_eq!(rec.redact_json(r#"{"token": }"#), r#"{"token": }"#);
        let kept = rec.keep_field("token");
        assert_eq!(
            kept.redact_json(r#"{"token":"cursor","password":"p"}"#),
            r#"{"token":"cursor","password":"[redacted]"}"#
        );
    }

    #[test]
    fn bodies_are_redacted_by_content_type_or_shape() {
        let rec = Record::new(Sink::Stdout);
        let form = b"user=ada&password=hunter2";
        assert_eq!(
            &*rec.redact_body(
                form,
                Some(b"application/x-www-form-urlencoded; charset=utf-8")
            ),
            b"user=ada&password=[redacted]"
        );
        // Without the form content type a body is not read as a form.
        assert_eq!(&*rec.redact_body(form, Some(b"text/plain")), form);
        let json = br#" {"password":"p"}"#;
        assert_eq!(
            &*rec.redact_body(json, None),
            br#" {"password":"[redacted]"}"#
        );
        assert_eq!(
            &*rec.redact_body(br#""password":"p""#, Some(b"application/vnd.api+json")),
            br#""password":"[redacted]""#
        );
        let binary = [0xffu8, b'{', b'"'];
        assert!(matches!(rec.redact_body(&binary, None), Cow::Borrowed(_)));
        // Not UTF-8, but JSON with a secret: redacted through a lossy view,
        // never kept as raw bytes.
        let mut bad = br#"{"name":"x"#.to_vec();
        bad.push(0xff);
        bad.extend_from_slice(br#"","token":"t"}"#);
        assert_eq!(
            &*rec.redact_body(&bad, Some(b"application/json")),
            "{\"name\":\"x\u{fffd}\",\"token\":\"[redacted]\"}".as_bytes()
        );
        let form = b"a=\xff&password=p";
        assert_eq!(
            &*rec.redact_body(form, Some(b"application/x-www-form-urlencoded")),
            "a=\u{fffd}&password=[redacted]".as_bytes()
        );
        // Without a secret the bytes are kept as they were.
        let plain = [b'{', 0xff, b'}'];
        assert!(matches!(rec.redact_body(&plain, None), Cow::Borrowed(_)));
    }

    #[test]
    fn headers_whose_name_says_secret_are_redacted() {
        let rec = Record::new(Sink::Stdout);
        for name in [
            "Authorization",
            "X-Auth-Token",
            "X-Session-Token",
            "Private-Token",
            "X-Goog-Api-Key",
            "X-Access-Token",
            "X-CSRF-Token",
            "X-Amz-Security-Token",
        ] {
            assert!(rec.secret(name), "{name} should be redacted");
        }
        for name in [
            "Content-Type",
            "Sec-WebSocket-Key",
            "Idempotency-Key",
            "WWW-Authenticate",
            "X-Request-Id",
            "Access-Control-Allow-Credentials",
        ] {
            assert!(!rec.secret(name), "{name} should be kept");
        }
        let kept = rec.keep_field("x-page-token").keep_field("cookie");
        assert!(!kept.secret("X-Page-Token"));
        assert!(
            kept.secret("Cookie"),
            "the six built-in headers stay redacted"
        );
        let mut out = String::new();
        kept.headers(
            &mut out,
            [
                ("X-Session-Token", &b"s3"[..]),
                ("X-Page-Token", b"p2"),
                ("Accept", b"*/*"),
            ]
            .into_iter(),
        );
        assert_eq!(
            out,
            r#""headers":[["x-session-token","[redacted]"],["x-page-token","p2"],["accept","*/*"]]"#
        );
    }

    #[test]
    fn multipart_parts_of_secrets_are_redacted() {
        let rec = Record::new(Sink::Stdout);
        let body = "--XyZ\r\n\
            Content-Disposition: form-data; name=\"user\"\r\n\r\n\
            ada\r\n\
            --XyZ\r\n\
            content-disposition: form-data; name=\"newPassword\"\r\n\r\n\
            hunter2\r\n\
            --XyZ\r\n\
            Content-Disposition: form-data; name=\"avatar\"; filename=\"token.png\"\r\n\
            Content-Type: image/png\r\n\r\n\
            PNG\r\n\
            --XyZ\r\n\
            Content-Disposition: form-data; name=api_key\r\n\r\n\
            AK-1\r\n\
            --XyZ--\r\n";
        let expected = body
            .replace("hunter2", "[redacted]")
            .replace("AK-1", "[redacted]");
        for ct in [
            "multipart/form-data; boundary=XyZ",
            "Multipart/Form-Data; charset=utf-8; boundary=\"XyZ\"",
        ] {
            assert_eq!(
                &*rec.redact_body(body.as_bytes(), Some(ct.as_bytes())),
                expected.as_bytes(),
                "{ct}"
            );
        }
        // The boundary is case-sensitive; without one nothing is read.
        let wrong = rec.redact_body(body.as_bytes(), Some(b"multipart/form-data; boundary=xyz"));
        assert!(matches!(wrong, Cow::Borrowed(_)));
        let none = rec.redact_body(body.as_bytes(), Some(b"multipart/form-data"));
        assert!(matches!(none, Cow::Borrowed(_)));
        // Cut short inside a secret part: redacted to the end.
        let cut = &body[..body.find("hunter2").unwrap() + 3];
        assert_eq!(
            rec.redact_multipart(cut, "XyZ"),
            format!("{}[redacted]", &body[..body.find("hunter2").unwrap()])
        );
        // An empty secret part and a body without the boundary stay.
        let empty = "--b\r\nContent-Disposition: form-data; name=\"token\"\r\n\r\n\r\n--b--";
        assert!(matches!(rec.redact_multipart(empty, "b"), Cow::Borrowed(_)));
        assert!(matches!(
            rec.redact_multipart("password=p", "b"),
            Cow::Borrowed(_)
        ));
        let kept = rec.keep_field("newpassword");
        assert_eq!(
            kept.redact_multipart(body, "XyZ"),
            body.replace("AK-1", "[redacted]")
        );
    }

    #[test]
    fn a_body_cut_inside_a_character_falls_back_to_base64() {
        let rec = Record::new(Sink::Stdout).max_body(2);
        let mut out = String::new();
        rec.body(&mut out, "é!".as_bytes(), 3, false);
        // "é" is two bytes: kept whole, as text.
        assert!(out.contains(r#""body":"é""#), "{out}");
        let mut out = String::new();
        rec.body(&mut out, "aé".as_bytes(), 3, false);
        // "a" plus half of "é" is not UTF-8.
        assert!(out.contains(r#""body_base64":"YcM=""#), "{out}");
        assert!(out.contains(r#""truncated":true"#), "{out}");
    }
}
