//! The dashboard's HTTP/1.1 server side: one request per connection, with
//! the checks that keep other web pages out.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// The largest request head read.
const MAX_HEAD: usize = 16 * 1024;
/// The largest request body read (the "send a request" box).
pub const MAX_BODY: usize = 2 << 20;
/// How long a client may take to send its request.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// A request to the dashboard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Req {
    pub method: String,
    /// The path, percent-decoded.
    pub path: String,
    /// Query parameters, percent-decoded.
    pub query: Vec<(String, String)>,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// An answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resp {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
    pub headers: Vec<(&'static str, String)>,
}

impl Resp {
    pub fn json(status: u16, v: &serde_json::Value) -> Resp {
        Resp {
            status,
            content_type: "application/json",
            body: v.to_string().into_bytes(),
            headers: Vec::new(),
        }
    }

    pub fn text(status: u16, t: impl Into<String>) -> Resp {
        Resp {
            status,
            content_type: "text/plain; charset=utf-8",
            body: t.into().into_bytes(),
            headers: Vec::new(),
        }
    }

    /// A JSON error the page shows: `{"error": {"code", "message", "fix"}}`.
    pub fn error(
        status: u16,
        code: &str,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) -> Resp {
        Resp::json(
            status,
            &serde_json::json!({"error": {"code": code, "message": message.into(), "fix": fix.into()}}),
        )
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decodes (and `+` as space when `form`); invalid escapes stay.
pub fn decode(s: &str, form: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' if form => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Why a request was refused before the handler.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Answered with this status and text.
    Status(u16, &'static str),
    /// An API request without this start's token: 403, coded.
    Token,
}

/// Whether two strings are equal, in a time that does not depend on where
/// they differ (the token compare).
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// A byte of an HTTP token (RFC 9110 §5.6.2).
fn is_token(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// Reads one request. `Err(None)` when the client went away.
pub fn read_request(stream: &mut impl Read) -> Result<Req, Option<Refusal>> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(Some(Refusal::Status(431, "the request head is too large")));
        }
        match stream.read(&mut chunk) {
            Ok(0) => return Err(None),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(Some(Refusal::Status(408, "the request took too long")))
            }
            Err(_) => return Err(None),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split(' ');
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(Some(Refusal::Status(400, "not an HTTP request")));
    };
    if !version.starts_with("HTTP/1.") || parts.next().is_some() || !target.starts_with('/') {
        return Err(Some(Refusal::Status(400, "not an HTTP/1.1 request")));
    }
    // A target is visible ASCII only (RFC 9112 §3.2): no control bytes,
    // spaces or bytes past ASCII.
    if !target.bytes().all(|b| (0x21..0x7f).contains(&b)) {
        return Err(Some(Refusal::Status(
            400,
            "the request target has bytes it may not have",
        )));
    }
    let mut headers = Vec::new();
    for line in lines {
        let Some((k, v)) = line.split_once(':') else {
            return Err(Some(Refusal::Status(400, "a header line without a colon")));
        };
        // A field name is a token with no space before the colon; a value
        // has no control bytes but tab (so no bare CR or LF either).
        if k.is_empty() || !k.bytes().all(is_token) {
            return Err(Some(Refusal::Status(
                400,
                "a header name that is not a token",
            )));
        }
        if v.bytes().any(|b| (b < 0x20 && b != b'\t') || b == 0x7f) {
            return Err(Some(Refusal::Status(
                400,
                "a header value with control bytes",
            )));
        }
        headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    // Fields the checks read once: two of them is a request built to
    // confuse (RFC 9112 §3.2 for Host).
    for name in [
        "host",
        "content-length",
        "origin",
        "sec-fetch-site",
        "x-giong-dashboard",
    ] {
        if headers.iter().filter(|(k, _)| k == name).count() > 1 {
            return Err(Some(Refusal::Status(
                400,
                "a header that may appear once is repeated",
            )));
        }
    }
    if headers.iter().any(|(k, _)| k == "transfer-encoding") {
        return Err(Some(Refusal::Status(
            400,
            "chunked request bodies are not accepted",
        )));
    }
    let len: usize = match headers.iter().find(|(k, _)| k == "content-length") {
        Some((_, v)) => v
            .parse()
            .map_err(|_| Some(Refusal::Status(400, "a bad Content-Length")))?,
        None => 0,
    };
    if len > MAX_BODY {
        return Err(Some(Refusal::Status(413, "the request body is too large")));
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        match stream.read(&mut chunk) {
            Ok(0) => return Err(None),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(Some(Refusal::Status(408, "the request took too long")))
            }
            Err(_) => return Err(None),
        }
    }
    body.truncate(len);
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };
    let query = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k, true), decode(v, true)),
            None => (decode(p, true), String::new()),
        })
        .collect();
    Ok(Req {
        method: method.to_string(),
        path: decode(path, false),
        query,
        headers,
        body,
    })
}

/// The checks that keep other web pages and other local programs out: see
/// the module page. `token` is this start's, which every API request sends
/// in `X-Giong-Dashboard`.
pub fn admit(req: &Req, port: u16, token: &str) -> Result<(), Refusal> {
    let host = req.header("host").unwrap_or("");
    let allowed = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(host)) {
        return Err(Refusal::Status(
            403,
            "the dashboard answers only at its loopback address",
        ));
    }
    if let Some(origin) = req.header("origin") {
        if !origin.eq_ignore_ascii_case(&format!("http://{host}")) {
            return Err(Refusal::Status(
                403,
                "requests from other origins are refused",
            ));
        }
    }
    if req
        .header("sec-fetch-site")
        .is_some_and(|s| s == "cross-site")
    {
        return Err(Refusal::Status(
            403,
            "requests from other sites are refused",
        ));
    }
    match req.method.as_str() {
        "GET" | "HEAD" | "POST" => {}
        _ => {
            return Err(Refusal::Status(
                405,
                "the dashboard takes GET and POST only",
            ))
        }
    }
    if req.path.starts_with("/api/")
        && !req
            .header("x-giong-dashboard")
            .is_some_and(|t| same(t, token))
    {
        return Err(Refusal::Token);
    }
    if req.method == "POST" && !req.path.starts_with("/api/") {
        return Err(Refusal::Status(405, "only the API takes POST"));
    }
    Ok(())
}

/// Writes an answer with the headers every answer gets.
pub fn write_response(
    stream: &mut impl Write,
    resp: &Resp,
    head_only: bool,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         X-Frame-Options: DENY\r\nCross-Origin-Resource-Policy: same-origin\r\n",
        resp.status,
        reason(resp.status),
        resp.content_type,
        resp.body.len()
    );
    for (k, v) in &resp.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    if !head_only {
        stream.write_all(&resp.body)?;
    }
    stream.flush()
}

/// Serves one connection: one request, then close.
pub fn serve_connection(
    mut stream: TcpStream,
    port: u16,
    token: &str,
    handler: &dyn Fn(&Req) -> Resp,
) {
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let req = match read_request(&mut stream) {
        Ok(r) => r,
        Err(None) => return,
        Err(Some(r)) => {
            let _ = write_response(&mut stream, &refusal(r), false);
            return;
        }
    };
    let resp = match admit(&req, port, token) {
        Ok(()) => {
            // A handler that panics answers 500 instead of leaving the page waiting.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&req))) {
                Ok(r) => r,
                Err(_) => Resp::error(
                    500,
                    "GIONG-E5054",
                    "the dashboard failed on this request",
                    "report it; the terminal shows the panic",
                ),
            }
        }
        Err(r) => refusal(r),
    };
    let _ = write_response(&mut stream, &resp, req.method == "HEAD");
}

fn refusal(r: Refusal) -> Resp {
    match r {
        Refusal::Status(s, why) => Resp::text(s, why),
        Refusal::Token => Resp::error(
            403,
            "GIONG-E5051",
            "the dashboard's API needs the token of the address giong printed",
            "open the address giong dashboard or giong dev --ui printed (it ends in /#t=...)",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(raw: &str) -> Result<Req, Option<Refusal>> {
        read_request(&mut raw.as_bytes())
    }

    fn get(path: &str, headers: &[(&str, &str)]) -> Req {
        Req {
            method: "GET".into(),
            path: path.into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Req::default()
        }
    }

    #[test]
    fn requests_built_to_confuse_are_refused() {
        let bad = |raw: &str| match req(raw) {
            Err(Some(Refusal::Status(400, _))) => {}
            other => panic!("{raw:?}: {:?}", other.map(|r| r.headers)),
        };
        // Two Host fields, however they are spelled or filled.
        bad("GET / HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nHost: evil.example\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nhost:\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHOST: 127.0.0.1:7700\r\nHost: 127.0.0.1:7700\r\n\r\n");
        bad("GET /api/state HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nX-Giong-Dashboard: 1\r\nX-Giong-Dashboard: 1\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nOrigin: http://127.0.0.1:7700\r\nOrigin: http://evil.example\r\n\r\n");
        bad("POST /api/send HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\nab");
        // Control bytes in the target, bare LF or CR in the head, a space
        // before the colon, a name that is not a token.
        bad("GET /a\x00b HTTP/1.1\r\nHost: 127.0.0.1:7700\r\n\r\n");
        bad("GET /a\x07 HTTP/1.1\r\nHost: 127.0.0.1:7700\r\n\r\n");
        bad("GET /\u{e9} HTTP/1.1\r\nHost: 127.0.0.1:7700\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHost: 127.0.0.1:7700\nX-Evil: 1\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHost: 127.0.0.1:7700\rX-Evil: 1\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHost : 127.0.0.1:7700\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHo(st: 127.0.0.1:7700\r\n\r\n");
        bad("GET / HTTP/1.1\r\n: x\r\nHost: 127.0.0.1:7700\r\n\r\n");
        bad("GET / HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nX-A: a\x00b\r\n\r\n");
        // A tab in a value and an encoded target are fine.
        let r =
            req("GET /a%20b?q=%00 HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nX-A: a\tb\r\n\r\n").unwrap();
        assert_eq!(r.header("x-a"), Some("a\tb"));
        assert!(admit(&r, 7700, "t").is_ok());
    }

    #[test]
    fn requests_parse() {
        let r = req("POST /api/send?a=1&b=x%20y+z&flag HTTP/1.1\r\nHost: 127.0.0.1:7700\r\nContent-Length: 3\r\n\r\nabcdef").unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/api/send");
        assert_eq!(r.param("a"), Some("1"));
        assert_eq!(r.param("b"), Some("x y z"));
        assert_eq!(r.param("flag"), Some(""));
        assert_eq!(r.body, b"abc");
        assert_eq!(r.header("host"), Some("127.0.0.1:7700"));
    }

    #[test]
    fn bad_requests_are_refused() {
        assert_eq!(req("GET / HTTP/1.1\r\n").unwrap_err(), None);
        assert!(matches!(
            req("GARBAGE\r\n\r\n"),
            Err(Some(Refusal::Status(400, _)))
        ));
        assert!(matches!(
            req("GET x HTTP/1.1\r\n\r\n"),
            Err(Some(Refusal::Status(400, _)))
        ));
        assert!(matches!(
            req("GET / HTTP/2\r\n\r\n"),
            Err(Some(Refusal::Status(400, _)))
        ));
        assert!(matches!(
            req("GET / HTTP/1.1\r\nbad\r\n\r\n"),
            Err(Some(Refusal::Status(400, _)))
        ));
        assert!(matches!(
            req("POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n"),
            Err(Some(Refusal::Status(400, _)))
        ));
        assert!(matches!(
            req("POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n"),
            Err(Some(Refusal::Status(400, _)))
        ));
        assert!(matches!(
            req("POST / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n"),
            Err(Some(Refusal::Status(413, _)))
        ));
        let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n", "a".repeat(MAX_HEAD + 10));
        assert!(matches!(req(&huge), Err(Some(Refusal::Status(431, _)))));
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(decode("a%2Fb%zz%4", false), "a/b%zz%4");
        assert_eq!(decode("a+b", false), "a+b");
        assert_eq!(decode("a+b", true), "a b");
        assert_eq!(decode("%", true), "%");
        assert_eq!(decode("%e2%9c%93", false), "✓");
    }

    #[test]
    fn other_sites_are_kept_out() {
        const T: &str = "5f3a9c2e8b41d70600010000000000aa";
        let ok = [("host", "127.0.0.1:7700"), ("x-giong-dashboard", T)];
        assert!(admit(&get("/api/state", &ok), 7700, T).is_ok());
        assert!(admit(&get("/", &[("host", "localhost:7700")]), 7700, T).is_ok());
        assert!(admit(&get("/", &[("host", "[::1]:7700")]), 7700, T).is_ok());
        // DNS rebinding: the page's host name is not ours.
        assert!(admit(&get("/", &[("host", "evil.example:7700")]), 7700, T).is_err());
        assert!(admit(&get("/", &[("host", "127.0.0.1:7701")]), 7700, T).is_err());
        assert!(admit(&get("/", &[]), 7700, T).is_err());
        // The API without the token (a plain cross-site form or img, or
        // another program on this machine that cannot read giong's output),
        // with the old constant, a wrong or a cut token.
        assert_eq!(
            admit(&get("/api/state", &[("host", "127.0.0.1:7700")]), 7700, T),
            Err(Refusal::Token)
        );
        for wrong in [
            "1",
            "",
            "5f3a9c2e8b41d70600010000000000ab",
            "5f3a9c2e8b41d706",
        ] {
            let h = [("host", "127.0.0.1:7700"), ("x-giong-dashboard", wrong)];
            assert_eq!(
                admit(&get("/api/state", &h), 7700, T),
                Err(Refusal::Token),
                "{wrong}"
            );
        }
        // The page itself needs no token: it holds no data.
        assert!(admit(&get("/", &[("host", "127.0.0.1:7700")]), 7700, T).is_ok());
        // Another origin.
        let mut other = ok.to_vec();
        other.push(("origin", "http://evil.example"));
        assert!(admit(&get("/api/state", &other), 7700, T).is_err());
        let mut same = ok.to_vec();
        same.push(("origin", "http://127.0.0.1:7700"));
        assert!(admit(&get("/api/state", &same), 7700, T).is_ok());
        let mut cross = ok.to_vec();
        cross.push(("sec-fetch-site", "cross-site"));
        assert!(admit(&get("/api/state", &cross), 7700, T).is_err());
        // Preflights and other methods.
        let mut opt = get("/api/send", &ok);
        opt.method = "OPTIONS".into();
        assert!(admit(&opt, 7700, T).is_err());
        let mut post = get("/", &[("host", "127.0.0.1:7700")]);
        post.method = "POST".into();
        assert!(admit(&post, 7700, T).is_err());
    }

    #[test]
    fn answers_carry_safety_headers() {
        let mut out = Vec::new();
        write_response(&mut out, &Resp::text(200, "hi"), false).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("HTTP/1.1 200 OK\r\n"));
        for h in [
            "Content-Length: 2",
            "Cache-Control: no-store",
            "X-Content-Type-Options: nosniff",
            "X-Frame-Options: DENY",
        ] {
            assert!(s.contains(h), "{s}");
        }
        assert!(
            !s.to_ascii_lowercase().contains("access-control-allow"),
            "{s}"
        );
        assert!(s.ends_with("\r\n\r\nhi"));
        let mut out = Vec::new();
        write_response(&mut out, &Resp::text(200, "hi"), true).unwrap();
        assert!(String::from_utf8(out).unwrap().ends_with("\r\n\r\n"));
    }
}
