//! A small HTTP/1.1 client for the service's own port: `giong record replay`
//! sends recorded requests with it and the dashboard's "send a request" box
//! goes through it. Standard library only, plain HTTP, one request per
//! connection (`Connection: close`).
//!
//! It is not a general client: no TLS, no redirects, no compression. The
//! service it talks to is the developer's own, on this machine.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// The largest answer read.
pub const MAX_ANSWER: usize = 16 << 20;

/// Where the service answers HTTP: `http://host:port` plus an optional path
/// prefix that is put in front of every request path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    /// `host:port`, as connected to and sent in `Host`.
    pub authority: String,
    /// `""` or `/prefix` (no trailing slash).
    pub prefix: String,
}

impl Base {
    /// `http://127.0.0.1:8080`, `127.0.0.1:8080` or `http://localhost:8080/api`.
    pub fn parse(text: &str) -> Result<Base, String> {
        let text = text.trim();
        if text.starts_with("https://") {
            return Err(format!(
                "{text} is HTTPS; giong talks plain HTTP to the service on this machine"
            ));
        }
        let rest = text.strip_prefix("http://").unwrap_or(text);
        if rest.contains("://") {
            return Err(format!("{text} is not an http:// address"));
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(format!("{text} has no host"));
        }
        if authority.contains('@') {
            return Err(format!(
                "{text}: user names in the address are not supported"
            ));
        }
        let authority = if has_port(authority) {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        Ok(Base {
            authority,
            prefix: path.trim_end_matches('/').to_string(),
        })
    }

    /// The address as `http://…`, for messages.
    pub fn url(&self) -> String {
        format!("http://{}{}", self.authority, self.prefix)
    }
}

fn has_port(authority: &str) -> bool {
    match authority.rfind(':') {
        // `[::1]` has colons inside the brackets only.
        Some(i) => !authority[i..].contains(']'),
        None => false,
    }
}

/// A request to send.
#[derive(Clone, Debug, Default)]
pub struct Outgoing {
    pub method: String,
    /// The path and query, starting with `/`.
    pub target: String,
    /// Sent as given, except `Host`, `Content-Length` and `Connection`, which
    /// the client writes itself.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// What came back.
#[derive(Clone, Debug, Default)]
pub struct Answer {
    pub status: u16,
    /// In order, names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub elapsed: Duration,
}

impl Answer {
    /// The first value of header `name` (lower case).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Why sending failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendError {
    /// Nothing listens there, or the connection broke before an answer.
    Connect(String),
    /// The request itself cannot be sent (a bad method or target).
    Invalid(String),
    /// An answer came that is not HTTP/1.1, or it was cut short.
    Answer(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Connect(s) | SendError::Invalid(s) | SendError::Answer(s) => f.write_str(s),
        }
    }
}

/// Header names the client writes itself.
const OWN_HEADERS: [&str; 5] = [
    "host",
    "content-length",
    "connection",
    "transfer-encoding",
    "keep-alive",
];

/// A method is a token (RFC 9110): letters, digits and a few marks.
fn valid_method(m: &str) -> bool {
    !m.is_empty()
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn valid_header_name(n: &str) -> bool {
    valid_method(n)
}

/// Sends `req` to `base` and reads the answer, within `timeout` overall.
pub fn send(base: &Base, req: &Outgoing, timeout: Duration) -> Result<Answer, SendError> {
    if !valid_method(&req.method) {
        return Err(SendError::Invalid(format!(
            "{:?} is not an HTTP method",
            req.method
        )));
    }
    if !req.target.starts_with('/') || req.target.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Err(SendError::Invalid(format!(
            "{:?} is not a request path: it must start with / and hold no spaces or control \
             characters (percent-encode them)",
            req.target
        )));
    }
    let mut head = format!(
        "{} {}{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        req.method, base.prefix, req.target, base.authority
    );
    for (name, value) in &req.headers {
        if OWN_HEADERS.iter().any(|h| name.eq_ignore_ascii_case(h)) {
            continue;
        }
        if !valid_header_name(name) {
            return Err(SendError::Invalid(format!("{name:?} is not a header name")));
        }
        if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(SendError::Invalid(format!(
                "the value of header {name} holds a line break"
            )));
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    let has_body = !req.body.is_empty() || matches!(req.method.as_str(), "POST" | "PUT" | "PATCH");
    if has_body {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("\r\n");

    let start = Instant::now();
    let addrs: Vec<_> = base
        .authority
        .to_socket_addrs()
        .map_err(|e| SendError::Connect(format!("{} cannot be resolved: {e}", base.authority)))?
        .collect();
    let mut last = None;
    let mut stream = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout.min(Duration::from_secs(5))) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = Some(e),
        }
    }
    let mut stream = stream.ok_or_else(|| {
        SendError::Connect(match last {
            Some(e) => format!("nothing answers at {}: {e}", base.url()),
            None => format!("{} resolves to no address", base.authority),
        })
    })?;
    let _ = stream.set_nodelay(true);
    let deadline = start + timeout;
    let remaining = |now: Instant| {
        deadline
            .saturating_duration_since(now)
            .max(Duration::from_millis(1))
    };
    stream
        .set_write_timeout(Some(remaining(Instant::now())))
        .map_err(|e| SendError::Connect(e.to_string()))?;
    stream
        .write_all(head.as_bytes())
        .and_then(|()| stream.write_all(&req.body))
        .map_err(|e| SendError::Connect(format!("sending to {} failed: {e}", base.url())))?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let head_only = req.method == "HEAD";
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(SendError::Answer(format!(
                "no whole answer within {} ms",
                timeout.as_millis()
            )));
        }
        stream
            .set_read_timeout(Some(remaining(now)))
            .map_err(|e| SendError::Connect(e.to_string()))?;
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(SendError::Answer(format!(
                    "no whole answer within {} ms",
                    timeout.as_millis()
                )))
            }
            Err(e) if raw.is_empty() => {
                return Err(SendError::Connect(format!(
                    "{} closed the connection: {e}",
                    base.url()
                )))
            }
            Err(e) => return Err(SendError::Answer(format!("reading the answer failed: {e}"))),
        };
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..n]);
        if raw.len() > MAX_ANSWER {
            return Err(SendError::Answer(format!(
                "the answer is larger than {} MiB",
                MAX_ANSWER >> 20
            )));
        }
        match complete(&raw, head_only) {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => return Err(SendError::Answer(e)),
        }
    }
    if raw.is_empty() {
        return Err(SendError::Connect(format!(
            "{} closed the connection without an answer",
            base.url()
        )));
    }
    let mut answer = parse(&raw, head_only).map_err(SendError::Answer)?;
    answer.elapsed = start.elapsed();
    Ok(answer)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The head of a response: status and headers.
fn parse_head(head: &[u8]) -> Result<(u16, Vec<(String, String)>), String> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(format!("not an HTTP/1.1 answer: {status_line:?}"));
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .filter(|s| (100..1000).contains(s))
        .ok_or_else(|| format!("not an HTTP/1.1 answer: {status_line:?}"))?;
    let mut headers = Vec::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            return Err(format!("a header line without a colon: {line:?}"));
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }
    Ok((status, headers))
}

enum Framing {
    None,
    Length(usize),
    Chunked,
    ToEnd,
}

fn framing(status: u16, headers: &[(String, String)], head_only: bool) -> Result<Framing, String> {
    if head_only || status < 200 || status == 204 || status == 304 {
        return Ok(Framing::None);
    }
    if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked"))
    {
        return Ok(Framing::Chunked);
    }
    match headers.iter().find(|(k, _)| k == "content-length") {
        Some((_, v)) => v
            .parse()
            .map(Framing::Length)
            .map_err(|_| format!("a bad Content-Length: {v:?}")),
        None => Ok(Framing::ToEnd),
    }
}

/// Whether `raw` holds a whole answer (an informational `1xx` head is skipped).
fn complete(raw: &[u8], head_only: bool) -> Result<bool, String> {
    let mut raw = raw;
    loop {
        let Some(end) = find(raw, b"\r\n\r\n") else {
            return Ok(false);
        };
        let (status, headers) = parse_head(&raw[..end])?;
        let rest = &raw[end + 4..];
        if (100..200).contains(&status) {
            raw = rest;
            continue;
        }
        return Ok(match framing(status, &headers, head_only)? {
            Framing::None => true,
            Framing::Length(n) => rest.len() >= n,
            Framing::Chunked => dechunk(rest).is_ok(),
            Framing::ToEnd => false,
        });
    }
}

/// Parses a whole answer.
pub fn parse(raw: &[u8], head_only: bool) -> Result<Answer, String> {
    let mut raw = raw;
    loop {
        let end = find(raw, b"\r\n\r\n").ok_or("the answer ended before its headers")?;
        let (status, headers) = parse_head(&raw[..end])?;
        let rest = &raw[end + 4..];
        if (100..200).contains(&status) {
            raw = rest;
            continue;
        }
        let body = match framing(status, &headers, head_only)? {
            Framing::None => Vec::new(),
            Framing::Length(n) => {
                if rest.len() < n {
                    return Err(format!(
                        "the answer was cut short: {} of {n} bytes",
                        rest.len()
                    ));
                }
                rest[..n].to_vec()
            }
            Framing::Chunked => dechunk(rest)?,
            Framing::ToEnd => rest.to_vec(),
        };
        return Ok(Answer {
            status,
            headers,
            body,
            elapsed: Duration::ZERO,
        });
    }
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = find(rest, b"\r\n").ok_or("a chunk size line is cut short")?;
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("a bad chunk size {size_text:?}"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            // Trailers, then the empty line.
            loop {
                let end = find(rest, b"\r\n").ok_or("the trailers are cut short")?;
                if end == 0 {
                    return Ok(out);
                }
                rest = &rest[end + 2..];
            }
        }
        if rest.len() < size + 2 {
            return Err("a chunk is cut short".into());
        }
        if &rest[size..size + 2] != b"\r\n" {
            return Err("a chunk does not end with CRLF".into());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn bases_parse() {
        let b = Base::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!(b.authority, "127.0.0.1:8080");
        assert_eq!(b.prefix, "");
        let b = Base::parse("localhost:3000/api/").unwrap();
        assert_eq!(b.authority, "localhost:3000");
        assert_eq!(b.prefix, "/api");
        assert_eq!(b.url(), "http://localhost:3000/api");
        assert_eq!(Base::parse("http://[::1]").unwrap().authority, "[::1]:80");
        assert_eq!(Base::parse("http://[::1]:9").unwrap().authority, "[::1]:9");
        assert!(Base::parse("https://x:1").is_err());
        assert!(Base::parse("ftp://x:1").is_err());
        assert!(Base::parse("http://").is_err());
        assert!(Base::parse("http://u:p@h:1").is_err());
    }

    #[test]
    fn answers_parse_with_length_chunks_trailers_and_1xx() {
        let a = parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nX-A: b\r\n\r\nhi",
            false,
        )
        .unwrap();
        assert_eq!((a.status, a.body.as_slice()), (200, &b"hi"[..]));
        assert_eq!(a.header("x-a"), Some("b"));
        let a = parse(
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n1;x=y\r\nc\r\n0\r\nT: 1\r\n\r\n",
            false,
        )
        .unwrap();
        assert_eq!((a.status, a.body.as_slice()), (201, &b"abc"[..]));
        let a = parse(
            b"HTTP/1.1 204 No Content\r\nContent-Length: 5\r\n\r\n",
            false,
        )
        .unwrap();
        assert!(a.body.is_empty());
        let a = parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\n", true).unwrap();
        assert!(a.body.is_empty());
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nab", false).is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n", false).is_err());
        assert!(parse(b"SSH-2.0-OpenSSH\r\n\r\n", false).is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nbad\r\n\r\n", false).is_err());
        assert!(parse(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nabX",
            false
        )
        .is_err());
    }

    #[test]
    fn completeness_follows_framing() {
        assert!(!complete(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nab", false).unwrap());
        assert!(complete(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc", false).unwrap());
        assert!(!complete(b"HTTP/1.1 100 Continue\r\n\r\n", false).unwrap());
        assert!(complete(b"HTTP/1.1 304 Not Modified\r\n\r\n", false).unwrap());
        assert!(!complete(b"HTTP/1.1 200 OK\r\n\r\nabc", false).unwrap());
        assert!(!complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n",
            false
        )
        .unwrap());
        assert!(complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\n0\r\n\r\n",
            false
        )
        .unwrap());
    }

    #[test]
    fn requests_with_injected_lines_are_refused() {
        let base = Base::parse("127.0.0.1:1").unwrap();
        let t = Duration::from_secs(1);
        let mut req = Outgoing {
            method: "GET".into(),
            target: "/a b".into(),
            ..Outgoing::default()
        };
        assert!(matches!(send(&base, &req, t), Err(SendError::Invalid(_))));
        req.target = "/a".into();
        req.method = "GE T".into();
        assert!(matches!(send(&base, &req, t), Err(SendError::Invalid(_))));
        req.method = "GET".into();
        req.headers = vec![("x".into(), "1\r\nEvil: 1".into())];
        assert!(matches!(send(&base, &req, t), Err(SendError::Invalid(_))));
        req.headers = vec![("x y".into(), "1".into())];
        assert!(matches!(send(&base, &req, t), Err(SendError::Invalid(_))));
    }

    #[test]
    fn a_closed_port_is_a_connect_error() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let base = Base::parse(&addr.to_string()).unwrap();
        let req = Outgoing {
            method: "GET".into(),
            target: "/".into(),
            ..Outgoing::default()
        };
        assert!(matches!(
            send(&base, &req, Duration::from_secs(2)),
            Err(SendError::Connect(_))
        ));
    }

    /// A one-shot server: answers the first request with `answer` and
    /// hands back what it received.
    fn serve_once(answer: &'static [u8]) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while find(&got, b"\r\n\r\n").is_none() {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            s.write_all(answer).unwrap();
            // Keep the connection open: the client must stop at the length.
            std::thread::sleep(Duration::from_millis(300));
            got
        });
        (addr, h)
    }

    #[test]
    fn a_request_goes_out_with_its_headers_and_the_answer_stops_at_its_length() {
        let (addr, h) =
            serve_once(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nLocation: /u/7\r\n\r\nok");
        let base = Base::parse(&format!("http://{addr}/api")).unwrap();
        let req = Outgoing {
            method: "POST".into(),
            target: "/u?x=1".into(),
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("Host".into(), "evil".into()),
                ("content-length".into(), "999".into()),
            ],
            body: b"{}".to_vec(),
        };
        let a = send(&base, &req, Duration::from_secs(5)).unwrap();
        assert_eq!(a.status, 201);
        assert_eq!(a.body, b"ok");
        assert_eq!(a.header("location"), Some("/u/7"));
        assert!(a.elapsed < Duration::from_millis(290), "{:?}", a.elapsed);
        let got = String::from_utf8(h.join().unwrap()).unwrap();
        assert!(got.starts_with("POST /api/u?x=1 HTTP/1.1\r\n"), "{got}");
        assert!(got.contains(&format!("Host: {addr}\r\n")), "{got}");
        assert!(got.contains("Content-Length: 2\r\n"), "{got}");
        assert!(!got.contains("evil") && !got.contains("999"), "{got}");
        assert!(got.contains("content-type: application/json\r\n"), "{got}");
    }

    #[test]
    fn a_silent_server_times_out() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = Base::parse(&l.local_addr().unwrap().to_string()).unwrap();
        let req = Outgoing {
            method: "GET".into(),
            target: "/".into(),
            ..Outgoing::default()
        };
        let start = Instant::now();
        let r = send(&base, &req, Duration::from_millis(300));
        assert!(matches!(r, Err(SendError::Answer(_))), "{r:?}");
        assert!(start.elapsed() < Duration::from_secs(3));
        drop(l);
    }
}
