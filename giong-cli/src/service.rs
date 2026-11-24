//! Finding a running service and asking its giong-mcp server, for `top`,
//! `routes`, `openapi` and the dashboard.
//!
//! Where the service is, first match wins:
//!
//! 1. `--mcp <address>`: `127.0.0.1:7777` or `unix:/path/to/socket`, with the
//!    token in `GIONG_MCP_TOKEN` (required on TCP, checked on a Unix socket
//!    too when the service has one);
//! 2. `GIONG_MCP` (and `GIONG_MCP_TOKEN`), as the service itself reads them;
//! 3. `.giong/dev.json` in the project, which `giong dev` writes for the run it
//!    is serving (`mcp_socket`, `mcp_token`).
//!
//! The client is plain HTTP/1.1 over a loopback socket or a Unix socket, one
//! request per connection, with the standard library only.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::code::Code;
use crate::project::find_manifest_dir;
use crate::report::Failure;

/// How long a call may take in all, connecting included: `profile` takes up
/// to 10 s.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// How long connecting may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The largest answer read: giong-mcp's own limit is 4 MiB.
const MAX_ANSWER: usize = 8 << 20;

/// Where a service's giong-mcp listens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Tcp(String),
    Unix(PathBuf),
}

impl Endpoint {
    /// `127.0.0.1:7777` or `unix:/path`.
    pub fn parse(text: &str) -> Result<Endpoint, Failure> {
        let text = text.trim();
        if let Some(path) = text.strip_prefix("unix:") {
            if path.is_empty() {
                return Err(bad_address(text));
            }
            return Ok(Endpoint::Unix(PathBuf::from(path)));
        }
        if text.is_empty() || !text.contains(':') {
            return Err(bad_address(text));
        }
        Ok(Endpoint::Tcp(text.to_string()))
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Tcp(a) => f.write_str(a),
            Endpoint::Unix(p) => write!(f, "unix:{}", p.display()),
        }
    }
}

fn bad_address(text: &str) -> Failure {
    Failure::new(
        Code::USAGE,
        format!("{text:?} is not a giong-mcp address"),
        "give an address such as 127.0.0.1:7777 or unix:/run/app/mcp.sock",
    )
}

/// A running service's giong-mcp server.
#[derive(Clone, Debug)]
pub struct Service {
    pub endpoint: Endpoint,
    pub token: Option<String>,
    /// Where it was found: `--mcp`, `GIONG_MCP` or `.giong/dev.json`.
    pub found_in: String,
}

/// What `giong dev` writes to `.giong/dev.json` while it serves a run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevState {
    pub pid: Option<u64>,
    pub mcp_socket: Option<PathBuf>,
    pub mcp_token: Option<String>,
    pub listen: Vec<String>,
    pub log: Option<PathBuf>,
}

impl DevState {
    /// Reads `.giong/dev.json` under `root`; `None` when there is none.
    pub fn read(root: &Path) -> Option<DevState> {
        let text = crate::state::read_to_string(&root.join(".giong").join("dev.json")).ok()?;
        let v: Value = serde_json::from_str(&text).ok()?;
        let path = |k: &str| v[k].as_str().filter(|s| !s.is_empty()).map(PathBuf::from);
        Some(DevState {
            pid: v["pid"].as_u64(),
            mcp_socket: path("mcp_socket"),
            mcp_token: v["mcp_token"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            listen: v["listen"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            log: path("log").map(|p| if p.is_relative() { root.join(p) } else { p }),
        })
    }
}

/// The project root of the current directory, if in one.
pub fn project_root() -> Option<PathBuf> {
    find_manifest_dir(&std::env::current_dir().ok()?)
}

/// Finds the service: `--mcp`, then `GIONG_MCP`, then `.giong/dev.json`.
pub fn locate(mcp_flag: Option<&str>) -> Result<Service, Failure> {
    let env_token = std::env::var("GIONG_MCP_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if let Some(addr) = mcp_flag {
        return Ok(Service {
            endpoint: Endpoint::parse(addr)?,
            token: env_token,
            found_in: "--mcp".into(),
        });
    }
    if let Some(addr) = std::env::var("GIONG_MCP").ok().filter(|a| !a.is_empty()) {
        return Ok(Service {
            endpoint: Endpoint::parse(&addr)?,
            token: env_token,
            found_in: "GIONG_MCP".into(),
        });
    }
    if let Some(root) = project_root() {
        if let Some(dev) = DevState::read(&root) {
            if let Some(sock) = dev.mcp_socket {
                return Ok(Service {
                    endpoint: Endpoint::Unix(sock),
                    token: dev.mcp_token,
                    found_in: ".giong/dev.json".into(),
                });
            }
        }
    }
    Err(Failure::new(
        Code::NO_SERVICE,
        "no running service to ask: no --mcp, no GIONG_MCP and no giong dev running here",
        "start the service with giong dev, or run it with GIONG_MCP=127.0.0.1:7777 and \
         GIONG_MCP_TOKEN set and pass --mcp 127.0.0.1:7777 (the service must keep \
         giong_mcp::start_from_env(&handle) alive)",
    ))
}

impl Service {
    /// Calls a tool and returns its structured answer.
    pub fn call(&self, tool: &str, arguments: Value) -> Result<Value, Failure> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": tool, "arguments": arguments },
        })
        .to_string();
        let reply = self.post(&body)?;
        if let Some(err) = reply.get("error") {
            let message = err["message"].as_str().unwrap_or("an error");
            // -32602: unknown tool (giong-mcp answers that for a tool its
            // build does not have).
            if err["code"] == -32602 && message.starts_with("unknown tool") {
                return Err(Failure::new(
                    Code::MCP_NO_TOOL,
                    format!("the service's giong-mcp has no {tool} tool: {message}"),
                    no_tool_fix(tool),
                ));
            }
            return Err(Failure::new(
                Code::MCP_TOOL_ERROR,
                format!("giong-mcp refused {tool}: {message}"),
                "check the call; giong-mcp's README lists the tools and their arguments",
            ));
        }
        let result = &reply["result"];
        if result["isError"] == true {
            let text = result["content"][0]["text"].as_str().unwrap_or("");
            if text.contains("unknown tool") || text.contains("no tool") {
                return Err(Failure::new(
                    Code::MCP_NO_TOOL,
                    format!("the service's giong-mcp has no {tool} tool: {text}"),
                    no_tool_fix(tool),
                ));
            }
            return Err(Failure::new(
                Code::MCP_TOOL_ERROR,
                format!("giong-mcp refused {tool}: {text}"),
                "check the call; giong-mcp's README lists the tools and their arguments",
            ));
        }
        match result.get("structuredContent") {
            Some(v) => Ok(v.clone()),
            None => Err(Failure::new(
                Code::MCP_TOOL_ERROR,
                format!("giong-mcp's answer to {tool} has no structuredContent"),
                "use a giong-mcp of the same version as this command",
            )),
        }
    }

    fn unreachable(&self, why: impl std::fmt::Display) -> Failure {
        Failure::new(
            Code::MCP_UNREACHABLE,
            format!(
                "the service's giong-mcp at {} (from {}) does not answer: {why}",
                self.endpoint, self.found_in
            ),
            "check that the service is running and keeps giong_mcp::start_from_env(&handle) \
             alive, and that the address and GIONG_MCP_TOKEN match the service's",
        )
    }

    /// One `POST /mcp`; returns the JSON-RPC reply.
    fn post(&self, body: &str) -> Result<Value, Failure> {
        let mut head = format!(
            "POST /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nAccept: application/json\r\n\
             MCP-Protocol-Version: 2025-06-18\r\nContent-Length: {}\r\n",
            body.len()
        );
        // On every transport: giong-mcp checks a token it was given on a
        // Unix socket too.
        if let Some(t) = &self.token {
            head.push_str(&format!("Authorization: Bearer {t}\r\n"));
        }
        head.push_str("\r\n");
        let deadline = Instant::now() + CALL_TIMEOUT;
        let raw = match &self.endpoint {
            Endpoint::Tcp(addr) => {
                let targets: Vec<SocketAddr> = addr
                    .to_socket_addrs()
                    .map_err(|e| self.unreachable(e))?
                    .collect();
                if targets.is_empty() {
                    return Err(self.unreachable("the address resolves to nothing"));
                }
                // Every address it resolves to (localhost: ::1, then 127.0.0.1).
                let mut last = None;
                let mut stream = None;
                for t in &targets {
                    match TcpStream::connect_timeout(t, CONNECT_TIMEOUT) {
                        Ok(s) => {
                            stream = Some(s);
                            break;
                        }
                        Err(e) => last = Some(e),
                    }
                }
                let Some(mut s) = stream else {
                    return Err(self.unreachable(
                        last.map_or_else(|| "no address".to_string(), |e| e.to_string()),
                    ));
                };
                exchange(&mut s, &head, body, deadline).map_err(|e| self.unreachable(e))?
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let mut s = connect_unix(path).map_err(|e| self.unreachable(e))?;
                exchange(&mut s, &head, body, deadline).map_err(|e| self.unreachable(e))?
            }
            #[cfg(not(unix))]
            Endpoint::Unix(_) => {
                return Err(self.unreachable("Unix sockets are not available on this system"))
            }
        };
        let (status, body) = parse_response(&raw).map_err(|e| self.unreachable(e))?;
        match status {
            200 => serde_json::from_slice(&body)
                .map_err(|e| self.unreachable(format!("the answer is not JSON: {e}"))),
            401 => Err(Failure::new(
                Code::MCP_UNREACHABLE,
                format!(
                    "the service's giong-mcp at {} refused the token (401)",
                    self.endpoint
                ),
                "set GIONG_MCP_TOKEN to the token the service was started with (on a Unix socket \
                 too, when the service has one)",
            )),
            other => Err(self.unreachable(format!(
                "HTTP {other}: {}",
                String::from_utf8_lossy(&body).trim()
            ))),
        }
    }
}

fn no_tool_fix(tool: &str) -> String {
    if tool == "routes" {
        "build the service with giong-mcp's web feature (giong-mcp = { version = \"1.5\", \
         features = [\"web\"] }) and giong-web 1.5 or newer"
            .into()
    } else {
        format!("use a giong-mcp version that has the {tool} tool")
    }
}

trait Stream: Read + Write {
    fn timeouts(&self, d: Duration) -> std::io::Result<()>;
}

impl Stream for TcpStream {
    fn timeouts(&self, d: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(d))?;
        self.set_write_timeout(Some(d))
    }
}

#[cfg(unix)]
impl Stream for std::os::unix::net::UnixStream {
    fn timeouts(&self, d: Duration) -> std::io::Result<()> {
        self.set_read_timeout(Some(d))?;
        self.set_write_timeout(Some(d))
    }
}

/// A Unix socket connection, given up on after [`CONNECT_TIMEOUT`] (std has
/// no timeout for it: a socket whose backlog is full blocks the connect).
#[cfg(unix)]
fn connect_unix(path: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    let (tx, rx) = std::sync::mpsc::channel();
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(std::os::unix::net::UnixStream::connect(p));
    });
    rx.recv_timeout(CONNECT_TIMEOUT).unwrap_or_else(|_| {
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("connecting timed out after {} s", CONNECT_TIMEOUT.as_secs()),
        ))
    })
}

/// The time left before `deadline`, or a "timed out" error.
fn left(deadline: Instant) -> std::io::Result<Duration> {
    let now = Instant::now();
    if now >= deadline {
        return Err(timed_out());
    }
    Ok(deadline - now)
}

fn timed_out() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("no whole answer within {} s", CALL_TIMEOUT.as_secs()),
    )
}

/// Sends the request and reads the answer, all within `deadline`: a server
/// that never answers, or trickles bytes, fails with "timed out", not an
/// OS error number.
fn exchange<S: Stream>(
    s: &mut S,
    head: &str,
    body: &str,
    deadline: Instant,
) -> std::io::Result<Vec<u8>> {
    let timeout = |e: std::io::Error| {
        if matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            timed_out()
        } else {
            e
        }
    };
    s.timeouts(left(deadline)?)?;
    s.write_all(head.as_bytes()).map_err(timeout)?;
    s.write_all(body.as_bytes()).map_err(timeout)?;
    let mut out = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    loop {
        if let Err(e) = s.timeouts(left(deadline)?) {
            // macOS refuses a timeout on a socket the peer has closed
            // (EINVAL); the one set above still bounds the read, which
            // returns at once.
            if e.raw_os_error().is_none() {
                return Err(e);
            }
        }
        let n = s.read(&mut chunk).map_err(timeout)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
        if out.len() > MAX_ANSWER {
            return Err(std::io::Error::other("the answer is larger than 8 MiB"));
        }
        if response_complete(&out) {
            break;
        }
    }
    Ok(out)
}

/// Whether `raw` holds a whole response with a `Content-Length`.
fn response_complete(raw: &[u8]) -> bool {
    let Some(end) = find(raw, b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..end]);
    content_length(&head).is_some_and(|n| raw.len() >= end + 4 + n)
}

fn content_length(head: &str) -> Option<usize> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())?
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The status and body of an HTTP/1.1 response (`Content-Length`,
/// chunked, or read to the end).
pub fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let end = find(raw, b"\r\n\r\n").ok_or("the answer ended before its headers")?;
    let head = String::from_utf8_lossy(&raw[..end]);
    let status_line = head.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("not an HTTP answer: {status_line:?}"))?;
    let rest = &raw[end + 4..];
    let chunked = head.lines().skip(1).any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });
    let body = if chunked {
        dechunk(rest)?
    } else if let Some(n) = content_length(&head) {
        if rest.len() < n {
            return Err(format!(
                "the answer was cut short: {} of {n} bytes",
                rest.len()
            ));
        }
        rest[..n].to_vec()
    } else {
        rest.to_vec()
    };
    Ok((status, body))
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = find(rest, b"\r\n").ok_or("a chunk size line is cut short")?;
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| format!("bad chunk size {size_text:?}"))?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err("a chunk is cut short".into());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_server_that_trickles_or_stays_silent_times_out() {
        use std::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            for (i, s) in l.incoming().enumerate() {
                let mut s = s.unwrap();
                std::thread::spawn(move || {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf);
                    // The first stays silent; the second sends a byte every
                    // 100 ms forever, which per-read timeouts never catch.
                    if i == 0 {
                        std::thread::sleep(Duration::from_secs(5));
                    } else {
                        let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\n\r\n");
                        while s.write_all(b"x").is_ok() {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    }
                });
            }
        });
        for _ in 0..2 {
            let mut s = TcpStream::connect(addr).unwrap();
            let start = Instant::now();
            let deadline = start + Duration::from_millis(600);
            let e = exchange(&mut s, "POST / HTTP/1.1\r\n\r\n", "", deadline).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::TimedOut, "{e}");
            assert!(e.to_string().contains("no whole answer within"), "{e}");
            assert!(start.elapsed() < Duration::from_secs(3));
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_token_goes_over_a_unix_socket_too() {
        // /tmp, not $TMPDIR: a socket path holds at most 104 bytes on macOS.
        let dir = PathBuf::from(format!("/tmp/gc-token-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock = dir.join("mcp.sock");
        let _ = std::fs::remove_file(&sock);
        let l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let server = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !String::from_utf8_lossy(&got).contains("\"jsonrpc\"") {
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
            }
            let body = r#"{"jsonrpc":"2.0","id":1,"result":{"structuredContent":{"ok":true}}}"#;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            String::from_utf8_lossy(&got).into_owned()
        });
        let svc = Service {
            endpoint: Endpoint::Unix(sock.clone()),
            token: Some("a-token-of-16-chars".into()),
            found_in: "--mcp".into(),
        };
        assert_eq!(
            svc.call("overview", json!({})).unwrap(),
            json!({"ok": true})
        );
        let request = server.join().unwrap();
        assert!(
            request.contains("Authorization: Bearer a-token-of-16-chars\r\n"),
            "{request}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn endpoints_parse() {
        assert_eq!(
            Endpoint::parse("127.0.0.1:7777").unwrap(),
            Endpoint::Tcp("127.0.0.1:7777".into())
        );
        assert_eq!(
            Endpoint::parse(" unix:/tmp/a.sock ").unwrap(),
            Endpoint::Unix("/tmp/a.sock".into())
        );
        assert_eq!(Endpoint::parse("unix:").unwrap_err().code, Code::USAGE);
        assert_eq!(Endpoint::parse("7777").unwrap_err().code, Code::USAGE);
        assert_eq!(Endpoint::parse("").unwrap_err().code, Code::USAGE);
        assert_eq!(Endpoint::parse("unix:/x").unwrap().to_string(), "unix:/x");
    }

    #[test]
    fn responses_parse_with_length_chunks_or_to_the_end() {
        let (s, b) =
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra").unwrap();
        assert_eq!((s, b.as_slice()), (200, &b"{}"[..]));
        let (s, b) = parse_response(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: Chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!((s, b.as_slice()), (200, &b"abcde"[..]));
        let (s, b) = parse_response(b"HTTP/1.1 401 Unauthorized\r\n\r\nno").unwrap();
        assert_eq!((s, b.as_slice()), (401, &b"no"[..]));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nab").is_err());
        assert!(parse_response(b"garbage").is_err());
        assert!(parse_response(b"SSH-2.0\r\n\r\n").is_err());
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err()
        );
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab")
                .is_err()
        );
    }

    #[test]
    fn completeness_needs_the_whole_body() {
        assert!(!response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nab"
        ));
        assert!(response_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc"
        ));
        assert!(!response_complete(b"HTTP/1.1 200 OK\r\n"));
    }

    #[test]
    fn dev_state_reads_what_giong_dev_writes() {
        let dir = std::env::temp_dir().join(format!("giong-cli-devstate-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".giong")).unwrap();
        std::fs::write(
            dir.join(".giong/dev.json"),
            r#"{"pid":42,"mcp_socket":"/tmp/x.sock","mcp_token":"","listen":["127.0.0.1:8080"],"log":".giong/dev.log"}"#,
        )
        .unwrap();
        let s = DevState::read(&dir).unwrap();
        assert_eq!(s.pid, Some(42));
        assert_eq!(s.mcp_socket, Some(PathBuf::from("/tmp/x.sock")));
        assert_eq!(s.mcp_token, None);
        assert_eq!(s.listen, ["127.0.0.1:8080"]);
        assert_eq!(s.log, Some(dir.join(".giong/dev.log")));
        std::fs::write(dir.join(".giong/dev.json"), "not json").unwrap();
        assert_eq!(DevState::read(&dir), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
