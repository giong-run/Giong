//! End-to-end tests: a real server on a real socket, driven by a blocking
//! client that speaks raw HTTP.

use std::cell::RefCell;
use std::future::Future;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong::time::sleep;
use giong_http::{Handler, Request, Response, Server};

// ------------------------------------------------------------------ harness

struct TestServer {
    addr: SocketAddr,
    handle: giong::Handle,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start<H>(server: Server, handler: H) -> Self
    where
        H: Handler + Clone + Send + Sync,
    {
        let rt = giong::Builder::new()
            .workers(1)
            .shutdown_grace(Duration::from_secs(20))
            .build()
            .unwrap();
        let handle = rt.handle();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            rt.run(move |_| {
                let (tx, server, handler) = (tx.clone(), server.clone(), handler.clone());
                async move {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    server.serve(listener, handler).await;
                }
            })
            .unwrap();
        });
        Self {
            addr: rx.recv().unwrap(),
            handle,
            thread: Some(thread),
        }
    }

    fn client(&self) -> Client {
        let stream = TcpStream::connect(self.addr).unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        Client {
            stream,
            buf: Vec::new(),
        }
    }

    /// Requests a shutdown and waits for the runtime to stop.
    fn stop(&mut self) -> Duration {
        let start = Instant::now();
        self.handle.shutdown();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        start.elapsed()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.head.lines().skip(1).find_map(|line| {
            let (n, v) = line.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap()
    }
}

struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    fn send(&mut self, bytes: impl AsRef<[u8]>) {
        self.stream.write_all(bytes.as_ref()).unwrap();
    }

    /// Whether the server has sent something (or closed), without waiting.
    fn answered(&self) -> bool {
        self.stream.set_nonblocking(true).unwrap();
        let peeked = self.stream.peek(&mut [0u8; 1]);
        self.stream.set_nonblocking(false).unwrap();
        !matches!(peeked, Err(e) if e.kind() == std::io::ErrorKind::WouldBlock)
    }

    /// Reads more input; `false` at end of stream (or on a reset).
    fn fill(&mut self) -> bool {
        let mut chunk = [0u8; 16 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => false,
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => false,
            Err(e) => panic!("read failed: {e}"),
        }
    }

    /// Reads one response. `head_only`: the response to a HEAD request.
    fn reply_to(&mut self, head_only: bool) -> Reply {
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            assert!(self.fill(), "connection closed before a full response head");
        };
        let head = String::from_utf8(self.buf[..end].to_vec()).unwrap();
        let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
        let mut reply = Reply {
            status,
            head,
            body: Vec::new(),
        };
        let len: usize = match reply.header("content-length") {
            Some(v) if !head_only => v.parse().unwrap(),
            _ => 0,
        };
        while self.buf.len() < end + len {
            assert!(self.fill(), "connection closed in the middle of a body");
        }
        reply.body = self.buf[end..end + len].to_vec();
        self.buf.drain(..end + len);
        reply
    }

    fn reply(&mut self) -> Reply {
        self.reply_to(false)
    }

    /// Asserts that the server closes the connection without sending more.
    fn expect_closed(&mut self) {
        while self.fill() {}
        assert!(
            self.buf.is_empty(),
            "unexpected data before close: {:?}",
            String::from_utf8_lossy(&self.buf)
        );
    }
}

/// A body large enough to be sent without copying it after its head.
const BIG: &[u8] = &[b'z'; 100_000];

/// Echoes what it understood of the request.
async fn echo(req: Request) -> Response {
    if req.path() == "/empty" {
        return Response::new(204);
    }
    if req.path() == "/big" {
        return Response::new(200).body(BIG);
    }
    let text = format!(
        "{} {} q={:?} v={} ua={:?} body={}",
        req.method(),
        req.path(),
        req.query(),
        req.version().as_str().trim_start_matches("HTTP/"),
        req.header_str("user-agent"),
        String::from_utf8_lossy(req.body()),
    );
    Response::text(text).header("X-Body-Len", req.body().len().to_string())
}

fn default_server() -> TestServer {
    TestServer::start(Server::new(), echo)
}

// -------------------------------------------------------------------- tests

#[test]
fn answers_requests_on_a_kept_alive_connection() {
    let server = default_server();
    let mut c = server.client();
    c.send("GET /hello?x=1&y=2 HTTP/1.1\r\nHost: test\r\nUSER-AGENT: t/1\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.status, 200);
    assert_eq!(
        r.text(),
        r#"GET /hello q=Some("x=1&y=2") v=1.1 ua=Some("t/1") body="#
    );
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert_eq!(r.header("connection"), None);
    let date = r.header("date").expect("a Date header");
    assert!(date.ends_with(" GMT") && date.len() == 29, "{date}");

    // Same connection, absolute-form target, empty lines before the request.
    c.send("\r\n\r\nGET http://test/abs/path?q HTTP/1.1\r\nHost: test\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.text(), r#"GET /abs/path q=Some("q") v=1.1 ua=None body="#);
}

#[test]
fn receives_bodies_with_a_length_and_chunked() {
    let server = default_server();
    let mut c = server.client();
    c.send("POST /a HTTP/1.1\r\nHost: t\r\nContent-Length: 11\r\n\r\nhello world");
    let r = c.reply();
    assert!(r.text().ends_with("body=hello world"), "{}", r.text());
    assert_eq!(r.header("x-body-len"), Some("11"));

    c.send(
        "POST /b HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n\
         5\r\nhello\r\n6;x=y\r\n world\r\n0\r\nTrailer: 1\r\n\r\n",
    );
    let r = c.reply();
    assert!(
        r.text().starts_with("POST /b ") && r.text().ends_with("body=hello world"),
        "{}",
        r.text()
    );

    // The connection is still in sync after the chunked body.
    c.send("GET /c HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(c.reply().text().starts_with("GET /c "));
}

#[test]
fn a_request_may_arrive_one_byte_at_a_time() {
    let server = default_server();
    let mut c = server.client();
    let request = "POST /slow HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
    for byte in request.bytes() {
        c.send([byte]);
        std::thread::sleep(Duration::from_micros(300));
    }
    assert!(c.reply().text().ends_with("body=abcde"));
}

#[test]
fn pipelined_requests_are_answered_in_order() {
    let server = default_server();
    let mut c = server.client();
    let mut batch = String::new();
    for i in 0..200 {
        batch.push_str(&format!("GET /p/{i} HTTP/1.1\r\nHost: t\r\n\r\n"));
    }
    c.send(batch);
    for i in 0..200 {
        let r = c.reply();
        assert!(
            r.text().starts_with(&format!("GET /p/{i} ")),
            "{}",
            r.text()
        );
    }
}

#[test]
fn large_bodies_survive_in_both_directions() {
    async fn reverse(req: Request) -> Response {
        let mut body = req.body().to_vec();
        body.reverse();
        Response::new(200).body(body)
    }
    let server = TestServer::start(Server::new().max_body_size(4 << 20), reverse);
    let mut c = server.client();
    let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    c.send(format!(
        "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n",
        body.len()
    ));
    c.send(&body);
    let r = c.reply();
    let mut expected = body;
    expected.reverse();
    assert!(r.body == expected);
}

#[test]
fn large_and_small_responses_mix_in_a_pipeline() {
    async fn sized(req: Request) -> Response {
        match req.path().strip_prefix("/owned/") {
            Some(len) => {
                let len: usize = len.parse().unwrap();
                Response::new(200).body((0..len).map(|i| (i % 251) as u8).collect::<Vec<_>>())
            }
            None => echo(req).await,
        }
    }
    let server = TestServer::start(Server::new(), sized);
    let mut c = server.client();
    // Around the size where bodies stop being copied, static and owned, with
    // HEAD and small responses in between, all in one batch.
    let paths = [
        "/small",
        "/big",
        "/owned/32767",
        "/owned/32768",
        "/small",
        "/owned/300000",
        "/big",
    ];
    let mut batch = String::new();
    for path in paths {
        batch.push_str(&format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n"));
    }
    batch.push_str("HEAD /big HTTP/1.1\r\nHost: t\r\n\r\n");
    batch.push_str("GET /big HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    c.send(batch);
    for path in paths {
        let r = c.reply();
        assert_eq!(r.status, 200, "{path}");
        match path {
            "/big" => assert!(r.body == BIG),
            "/small" => assert!(r.text().starts_with("GET /small "), "{}", r.text()),
            owned => {
                let len: usize = owned["/owned/".len()..].parse().unwrap();
                assert!(r.body.iter().copied().eq((0..len).map(|i| (i % 251) as u8)));
            }
        }
    }
    let head = c.reply_to(true);
    assert_eq!(head.header("content-length"), Some("100000"));
    let last = c.reply();
    assert_eq!(last.header("connection"), Some("close"));
    assert!(last.body == BIG);
    c.expect_closed();
}

/// A file under the temporary directory holding `data`, removed on drop.
struct TempFile(std::path::PathBuf);

impl TempFile {
    fn new(name: &str, data: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!("giong-http-{}-{name}", std::process::id()));
        std::fs::write(&path, data).unwrap();
        Self(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn file_data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 253) as u8).collect()
}

/// Serves `/file` (all of it), `/range` (bytes 100..1100), `/small`
/// (a 10-byte file), `/short` (100 bytes more than the file has) and
/// anything else as `echo` does.
fn file_handler(
    path: std::path::PathBuf,
) -> impl Fn(Request) -> std::pin::Pin<Box<dyn Future<Output = Response>>> + Clone + Send + Sync {
    move |req: Request| {
        let path = path.clone();
        Box::pin(async move {
            let open = || giong::fs::File::open(&path);
            match req.path() {
                "/file" => {
                    Response::new(200).body(giong_http::Body::file(open().await.unwrap()).unwrap())
                }
                "/range" => Response::new(206).body(giong_http::Body::file_range(
                    open().await.unwrap(),
                    100,
                    1000,
                )),
                "/small" => Response::new(200).body(giong_http::Body::file_range(
                    open().await.unwrap(),
                    5,
                    10,
                )),
                "/short" => {
                    let file = open().await.unwrap();
                    let len = file.len().unwrap();
                    Response::new(200).body(giong_http::Body::file_range(file, 0, len + 100))
                }
                _ => echo(req).await,
            }
        })
    }
}

#[test]
fn file_bodies_are_sent_from_the_file() {
    let data = file_data(3_000_017);
    let tmp = TempFile::new("file-body", &data);
    let server = TestServer::start(Server::new(), file_handler(tmp.0.clone()));
    let mut c = server.client();
    // In one batch, between other responses, and after a HEAD.
    let mut batch = String::new();
    for path in ["/file", "/x", "/range", "/small"] {
        batch.push_str(&format!("GET {path} HTTP/1.1\r\nHost: t\r\n\r\n"));
    }
    batch.push_str("HEAD /file HTTP/1.1\r\nHost: t\r\n\r\n");
    batch.push_str("GET /file HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    c.send(batch);
    let r = c.reply();
    assert_eq!(r.header("content-length"), Some("3000017"));
    assert!(r.body == data);
    assert!(c.reply().text().starts_with("GET /x "));
    let r = c.reply();
    assert_eq!(r.status, 206);
    assert!(r.body == data[100..1100]);
    assert!(c.reply().body == data[5..15]);
    let head = c.reply_to(true);
    assert_eq!(head.header("content-length"), Some("3000017"));
    let last = c.reply();
    assert_eq!(last.header("connection"), Some("close"));
    assert!(last.body == data);
    c.expect_closed();
}

#[test]
fn a_file_shorter_than_announced_closes_the_connection() {
    let data = file_data(200_000);
    let tmp = TempFile::new("file-short", &data);
    let server = TestServer::start(Server::new(), file_handler(tmp.0.clone()));
    let mut c = server.client();
    c.send("GET /short HTTP/1.1\r\nHost: t\r\n\r\nGET /x HTTP/1.1\r\nHost: t\r\n\r\n");
    // The head promised 100 bytes more than the file had: the file, then
    // the end of the connection, and no answer to the second request.
    while !c.buf.windows(4).any(|w| w == b"\r\n\r\n") {
        assert!(c.fill());
    }
    while c.fill() {}
    let text = String::from_utf8_lossy(&c.buf).into_owned();
    assert!(text.contains("Content-Length: 200100\r\n"), "{text:.200}");
    let end = c.buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(c.buf[end..] == data[..], "the file and nothing else");
}

#[test]
fn keep_alive_follows_the_protocol_version_and_connection_header() {
    let server = default_server();

    // HTTP/1.1 with `Connection: close`.
    let mut c = server.client();
    c.send("GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.header("connection"), Some("close"));
    c.expect_closed();

    // HTTP/1.0 closes by default and needs no Host.
    let mut c = server.client();
    c.send("GET / HTTP/1.0\r\n\r\n");
    let r = c.reply();
    assert!(r.head.starts_with("HTTP/1.0 200 OK\r\n"), "{}", r.head);
    assert!(r.text().contains("v=1.0"));
    c.expect_closed();

    // HTTP/1.0 with keep-alive stays open, and says so.
    let mut c = server.client();
    c.send("GET /1 HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    assert_eq!(c.reply().header("connection"), Some("keep-alive"));
    c.send("GET /2 HTTP/1.0\r\nConnection: keep-alive\r\n\r\n");
    assert!(c.reply().text().starts_with("GET /2 "));
}

#[test]
fn a_successful_answer_to_connect_closes_the_connection() {
    // After a 2xx to CONNECT, the client and every proxy on the way treat
    // the connection as a tunnel (RFC 9110, section 9.3.6), but the server
    // has none to give: bytes that a proxy in front passes on blindly as
    // tunnel data must not be read as requests it never saw.
    let server = default_server();
    let mut c = server.client();
    c.send(
        "CONNECT db.internal:5432 HTTP/1.1\r\nHost: db.internal:5432\r\n\r\n\
         GET /admin HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    let r = c.reply();
    assert_eq!(r.status, 200);
    assert_eq!(r.header("connection"), Some("close"));
    c.expect_closed();

    // A refusal leaves the connection as it was.
    let refuse = |req: Request| async move {
        if req.method() == "CONNECT" {
            Response::new(405)
        } else {
            Response::text("ok")
        }
    };
    let server = TestServer::start(Server::new(), refuse);
    let mut c = server.client();
    c.send(
        "CONNECT db.internal:5432 HTTP/1.1\r\nHost: db.internal:5432\r\n\r\n\
         GET / HTTP/1.1\r\nHost: t\r\n\r\n",
    );
    assert_eq!(c.reply().status, 405);
    assert_eq!(c.reply().text(), "ok");
}

#[test]
fn head_and_bodiless_statuses_send_no_body() {
    let server = default_server();
    let mut c = server.client();
    c.send("HEAD /h HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.reply_to(true);
    assert_eq!(r.status, 200);
    let len: usize = r.header("content-length").unwrap().parse().unwrap();
    assert!(len > 0);

    c.send("GET /empty HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.status, 204);
    assert_eq!(r.header("content-length"), None);

    // Nothing was left over from the HEAD response.
    c.send("GET /after HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(c.reply().text().starts_with("GET /after "));
}

#[test]
fn malformed_requests_get_an_error_and_the_connection_is_closed() {
    let server = default_server();
    let cases: &[(&str, u16)] = &[
        ("GET / HTTP/1.1\r\n\r\n", 400),
        ("GET /\r\n\r\n", 400),
        ("GET / HTTP/2.0\r\nHost: t\r\n\r\n", 505),
        ("GET / HTTP/1.1\r\nHost: t\r\nName : v\r\n\r\n", 400),
        ("GET / HTTP/1.1\r\nHost: t\r\n folded\r\n\r\n", 400),
        ("GET / HTTP/1.1\nHost: t\n\r\n\r\n", 400),
        (
            "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
            400,
        ),
        (
            "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\nContent-Length: 4\r\n\r\nabcd",
            400,
        ),
        (
            "POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: gzip\r\n\r\n",
            501,
        ),
        (
            "POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
            400,
        ),
        ("GET / HTTP/1.1\r\nHost: t\r\nExpect: magic\r\n\r\n", 417),
    ];
    for (request, status) in cases {
        let mut c = server.client();
        c.send(request);
        let r = c.reply();
        assert_eq!(r.status, *status, "{request:?}");
        assert_eq!(r.header("connection"), Some("close"), "{request:?}");
        c.expect_closed();
    }

    // A pipelined request after a bad one is never executed.
    let mut c = server.client();
    c.send("GET /ok HTTP/1.1\r\nHost: t\r\n\r\nBAD\r\n\r\nGET /never HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().status, 200);
    assert_eq!(c.reply().status, 400);
    c.expect_closed();
}

#[test]
fn size_limits_are_enforced_and_the_error_is_readable() {
    let server = TestServer::start(Server::new().max_head_size(512).max_body_size(1000), echo);

    let mut c = server.client();
    c.send(format!(
        "GET / HTTP/1.1\r\nHost: t\r\nX-Big: {}\r\n\r\n",
        "a".repeat(600)
    ));
    assert_eq!(c.reply().status, 431);
    c.expect_closed();

    // A head that never ends is cut off at the limit as well.
    let mut c = server.client();
    c.send(format!("GET /{}", "a".repeat(700)));
    assert_eq!(c.reply().status, 431);

    // Declared length over the limit: refused before the body is read, and
    // the client can still read the answer although it keeps sending.
    let mut c = server.client();
    c.send("POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 200000\r\n\r\n");
    let _ = c.stream.write_all(&vec![b'x'; 200_000]);
    assert_eq!(c.reply().status, 413);
    c.expect_closed();

    // Chunked bodies are counted as they arrive.
    let mut c = server.client();
    c.send("POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n");
    c.send(format!("3e8\r\n{}\r\n", "x".repeat(1000)));
    c.send("1\r\nx\r\n0\r\n\r\n");
    assert_eq!(c.reply().status, 413);

    // Exactly at the limit is fine.
    let mut c = server.client();
    c.send(format!(
        "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 1000\r\n\r\n{}",
        "y".repeat(1000)
    ));
    let r = c.reply();
    assert_eq!((r.status, r.header("x-body-len")), (200, Some("1000")));
}

#[test]
fn slow_requests_time_out_and_idle_connections_are_closed() {
    let server = TestServer::start(
        Server::new()
            .request_timeout(Duration::from_millis(150))
            .idle_timeout(Duration::from_millis(300)),
        echo,
    );

    // Half a head, then silence.
    let mut c = server.client();
    let start = Instant::now();
    c.send("GET / HTTP/1.1\r\nHost:");
    let r = c.reply();
    assert_eq!(r.status, 408);
    assert!(start.elapsed() >= Duration::from_millis(150));
    c.expect_closed();

    // Half a body, then silence.
    let mut c = server.client();
    c.send("POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 10\r\n\r\nabc");
    assert_eq!(c.reply().status, 408);

    // A slow drip does not reset the clock. The drip stops once the answer
    // is there: the server discards input for only so long after it, and a
    // line that arrives once it has closed makes its kernel answer with a
    // reset, which macOS lets destroy the answer before it is read (seen on
    // a slow runner, where 40 sleeps of 20 ms outlasted that time).
    let mut c = server.client();
    let start = Instant::now();
    c.send("GET / HTTP/1.1\r\n");
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(20));
        if c.answered() || c.stream.write_all(b"X-Drip: 1\r\n").is_err() {
            break;
        }
    }
    assert_eq!(c.reply().status, 408);
    assert!(start.elapsed() < Duration::from_secs(5));

    // An idle connection is closed silently.
    let mut c = server.client();
    let start = Instant::now();
    c.expect_closed();
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert!(start.elapsed() < Duration::from_secs(5));

    // A connection that is used stays open past the idle timeout in total.
    // The pauses are a sixth of the idle timeout: macOS lets a sleeping
    // thread wake late by up to the length of its sleep (a 150 ms sleep
    // took 300 to 310 ms on GitHub's macOS runners), so half the timeout
    // was not enough margin.
    let mut c = server.client();
    let start = Instant::now();
    let mut i = 0;
    while start.elapsed() < Duration::from_millis(600) {
        std::thread::sleep(Duration::from_millis(50));
        c.send(format!("GET /{i} HTTP/1.1\r\nHost: t\r\n\r\n"));
        assert_eq!(c.reply().status, 200);
        i += 1;
    }
}

#[test]
fn blank_lines_do_not_keep_a_connection_idle_for_ever() {
    let server = TestServer::start(
        Server::new()
            .request_timeout(Duration::from_millis(300))
            .idle_timeout(Duration::from_secs(30)),
        echo,
    );

    // A CRLF every 50 ms never lets the idle timeout run out; only the
    // request timeout can end it.
    let mut c = server.client();
    let start = Instant::now();
    for _ in 0..200 {
        if c.stream.write_all(b"\r\n").is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        if start.elapsed() > Duration::from_secs(2) {
            break;
        }
    }
    assert_eq!(c.reply().status, 408);
    assert!(start.elapsed() < Duration::from_secs(5));

    // A stray CRLF after a body, as some clients send, leaves the
    // connection idle and usable.
    let mut c = server.client();
    c.send("POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\nhi\r\n");
    assert_eq!(c.reply().status, 200);
    std::thread::sleep(Duration::from_millis(600));
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().status, 200);
}

#[test]
fn expect_continue_is_answered_before_the_body() {
    let server = default_server();
    let mut c = server.client();
    c.send("POST /e HTTP/1.1\r\nHost: t\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\n");
    while !c.buf.ends_with(b"\r\n\r\n") {
        assert!(c.fill());
    }
    assert_eq!(c.buf, b"HTTP/1.1 100 Continue\r\n\r\n");
    c.buf.clear();
    c.send("data");
    let r = c.reply();
    assert!(r.status == 200 && r.text().ends_with("body=data"));

    // If the body is already there, no interim response is sent.
    c.send("POST /e HTTP/1.1\r\nHost: t\r\nContent-Length: 4\r\nExpect: 100-continue\r\n\r\nmore");
    let r = c.reply();
    assert!(r.status == 200 && r.text().ends_with("body=more"));
}

#[test]
fn a_waiting_handler_does_not_hold_back_earlier_responses() {
    async fn handler(req: Request) -> Response {
        if req.path() == "/slow" {
            sleep(Duration::from_millis(400)).await;
        }
        Response::text(req.path().to_string())
    }
    let server = TestServer::start(Server::new(), handler);
    let mut c = server.client();
    let start = Instant::now();
    c.send("GET /fast HTTP/1.1\r\nHost: t\r\n\r\nGET /slow HTTP/1.1\r\nHost: t\r\n\r\nGET /last HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().text(), "/fast");
    assert!(
        start.elapsed() < Duration::from_millis(300),
        "the first response waited for the slow handler: {:?}",
        start.elapsed()
    );
    assert_eq!(c.reply().text(), "/slow");
    assert!(start.elapsed() >= Duration::from_millis(400));
    assert_eq!(c.reply().text(), "/last");
}

#[test]
fn a_panicking_handler_gets_500_and_only_its_connection_closes() {
    #[derive(Clone)]
    struct Panics;
    impl Handler for Panics {
        fn stream_body(&self, req: &Request) -> Option<u64> {
            if req.path() == "/panic-deciding" {
                panic!("stream_body panic (expected by this test)");
            }
            req.path().starts_with("/stream").then_some(1 << 20)
        }

        async fn call(&self, req: Request) -> Response {
            if req.path().contains("panic") {
                panic!("handler panic (expected by this test)");
            }
            Response::text("fine")
        }
    }
    let server = TestServer::start(Server::new(), Panics);
    let mut other = server.client();
    other.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(other.reply().text(), "fine");

    for request in [
        // A request behind it is not answered: the connection closes.
        "GET /panic HTTP/1.1\r\nHost: t\r\n\r\nGET / HTTP/1.1\r\nHost: t\r\n\r\n",
        "POST /stream-panic HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n\r\nhello",
        "POST /panic-deciding HTTP/1.1\r\nHost: t\r\nContent-Length: 5\r\n\r\nhello",
    ] {
        let mut c = server.client();
        c.send(request);
        let reply = c.reply();
        assert_eq!(reply.status, 500, "{request}");
        assert_eq!(reply.header("connection"), Some("close"), "{request}");
        c.expect_closed();
    }

    other.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(other.reply().text(), "fine");
    assert_eq!(server.client().stream.peer_addr().unwrap(), server.addr);
}

thread_local! {
    static KEPT: RefCell<Vec<Request>> = const { RefCell::new(Vec::new()) };
}

#[test]
fn requests_kept_by_the_handler_stay_valid() {
    // The handler stores every request. The connection must then stop reusing
    // the buffer those requests point into.
    async fn hoard(req: Request) -> Response {
        KEPT.with(|kept| {
            let mut kept = kept.borrow_mut();
            kept.push(req);
            let seen: Vec<String> = kept
                .iter()
                .map(|r| format!("{}:{}", r.path(), String::from_utf8_lossy(r.body())))
                .collect();
            Response::text(seen.join(","))
        })
    }
    let server = TestServer::start(Server::new(), hoard);
    let mut c = server.client();
    let mut expected = Vec::new();
    for i in 0..30 {
        let body = format!("body-{i}-{}", "z".repeat(i * 7));
        c.send(format!(
            "POST /k{i} HTTP/1.1\r\nHost: t\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ));
        expected.push(format!("/k{i}:{body}"));
        assert_eq!(c.reply().text(), expected.join(","));
    }
    // Pipelined as well: several requests share one buffer.
    let mut batch = String::new();
    for i in 30..40 {
        batch.push_str(&format!(
            "POST /k{i} HTTP/1.1\r\nHost: t\r\nContent-Length: 1\r\n\r\n{}",
            i % 10
        ));
    }
    c.send(batch);
    for i in 30..40 {
        expected.push(format!("/k{i}:{}", i % 10));
        assert_eq!(c.reply().text(), expected.join(","));
    }
}

#[test]
fn shutdown_finishes_requests_in_progress_and_closes_idle_connections() {
    async fn handler(req: Request) -> Response {
        if req.path() == "/slow" {
            sleep(Duration::from_millis(400)).await;
        }
        Response::text("done")
    }
    let mut server = TestServer::start(Server::new(), handler);
    let mut idle = server.client();
    idle.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(idle.reply().text(), "done");

    let mut busy = server.client();
    let sent = Instant::now();
    busy.send("GET /slow HTTP/1.1\r\nHost: t\r\n\r\n");
    std::thread::sleep(Duration::from_millis(100));
    // A loaded machine sleeps longer, and the request is that much nearer
    // its end when the server is told to stop.
    let waited = sent.elapsed();

    let stopped_after = server.stop();

    // The request in progress was answered, and told to go away.
    let r = busy.reply();
    assert_eq!((r.status, r.text()), (200, "done"));
    assert_eq!(r.header("connection"), Some("close"));
    busy.expect_closed();
    idle.expect_closed();
    // The runtime waited for the slow request, not for the 20 s grace period.
    assert!(
        waited + stopped_after >= Duration::from_millis(350),
        "{stopped_after:?} after {waited:?}"
    );
    assert!(stopped_after < Duration::from_secs(10), "{stopped_after:?}");
    assert!(TcpStream::connect(server.addr).is_err());
}

#[test]
fn many_connections_at_once() {
    let server = default_server();
    let addr = server.addr;
    let threads: Vec<_> = (0..16)
        .map(|t| {
            std::thread::spawn(move || {
                for round in 0..25 {
                    let stream = TcpStream::connect(addr).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut c = Client {
                        stream,
                        buf: Vec::new(),
                    };
                    for i in 0..10 {
                        c.send(format!("GET /{t}/{round}/{i} HTTP/1.1\r\nHost: t\r\n\r\n"));
                        let r = c.reply();
                        assert!(r.text().starts_with(&format!("GET /{t}/{round}/{i} ")));
                    }
                    let _ = c.stream.flush();
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
}

/// At the limit, an idle connection over a Unix socket makes room too.
#[cfg(unix)]
#[test]
fn at_the_connection_limit_an_idle_unix_connection_makes_room() {
    use std::os::unix::net::UnixStream;

    let path = std::env::temp_dir().join(format!("giong-http-evict-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let (tx, rx) = mpsc::channel();
    let server_path = path.clone();
    let thread = std::thread::spawn(move || {
        rt.run(move |_| {
            let (tx, path) = (tx.clone(), server_path.clone());
            async move {
                let listener = giong::net::UnixListener::bind(path).unwrap();
                tx.send(()).unwrap();
                Server::new()
                    .max_connections(1)
                    .serve_unix(listener, echo)
                    .await;
            }
        })
        .unwrap();
    });
    rx.recv().unwrap();

    let ask = |stream: &mut UnixStream, what: &str| {
        stream
            .write_all(format!("GET /{what} HTTP/1.1\r\nHost: t\r\n\r\n").as_bytes())
            .unwrap();
        let mut got = [0u8; 4096];
        let n = stream.read(&mut got).unwrap();
        String::from_utf8_lossy(&got[..n]).into_owned()
    };
    let mut first = UnixStream::connect(&path).unwrap();
    first
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert!(ask(&mut first, "first").contains("GET /first "));
    let mut second = UnixStream::connect(&path).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert!(ask(&mut second, "second").contains("GET /second "));
    // The first connection was closed to make room.
    assert_eq!(first.read(&mut [0u8; 64]).unwrap(), 0);

    handle.shutdown();
    thread.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn serves_over_a_unix_socket() {
    use std::os::unix::net::UnixStream;

    let path = std::env::temp_dir().join(format!("giong-http-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let rt = giong::Builder::new()
        .workers(1)
        .shutdown_grace(Duration::from_secs(20))
        .build()
        .unwrap();
    let handle = rt.handle();
    let (tx, rx) = mpsc::channel();
    let server_path = path.clone();
    let thread = std::thread::spawn(move || {
        rt.run(move |_| {
            let (tx, path) = (tx.clone(), server_path.clone());
            async move {
                let listener = giong::net::UnixListener::bind(path).unwrap();
                tx.send(()).unwrap();
                Server::new().serve_unix(listener, echo).await;
            }
        })
        .unwrap();
    });
    rx.recv().unwrap();

    let mut stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // Three requests on one connection, the last one closing it.
    stream
        .write_all(
            b"POST /unix HTTP/1.1\r\nHost: t\r\nContent-Length: 2\r\n\r\nhi\
              GET /big HTTP/1.1\r\nHost: t\r\n\r\n\
              GET /bye HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut all = String::new();
    stream.read_to_string(&mut all).unwrap();
    assert_eq!(all.matches("HTTP/1.1 200 OK\r\n").count(), 3, "{all}");
    let big = std::str::from_utf8(BIG).unwrap();
    assert!(
        all.contains(&format!(
            "Content-Length: 100000\r\n\r\n{big}HTTP/1.1 200 OK\r\n"
        )),
        "the large body is not whole, or not in its place"
    );
    assert!(
        all.contains("POST /unix q=None v=1.1 ua=None body=hi"),
        "{all}"
    );
    assert!(all.contains("Connection: close\r\n"), "{all}");
    assert!(
        all.ends_with("GET /bye q=None v=1.1 ua=None body="),
        "{all}"
    );

    // An idle connection is closed by the shutdown.
    let mut idle = UnixStream::connect(&path).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    handle.shutdown();
    assert_eq!(idle.read(&mut [0u8; 8]).unwrap(), 0);
    thread.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn handlers_see_the_client_address() {
    async fn peer(req: Request) -> Response {
        Response::text(format!("{:?}", req.peer_addr()))
    }
    let server = TestServer::start(Server::new(), peer);
    let mut c = server.client();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    let expected = format!("{:?}", Some(c.stream.local_addr().unwrap()));
    assert_eq!(c.reply().text(), expected);
}

/// On Windows idle connections are not closed for new clients: they wait.
#[cfg(windows)]
#[test]
fn at_the_connection_limit_new_clients_wait_their_turn() {
    let server = TestServer::start(Server::new().max_connections(1), echo);
    let mut first = server.client();
    first.send("GET /first HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(first.reply().status, 200);

    // The kernel completes the second handshake, but the server does not
    // accept the connection while the first one is open.
    let mut second = server.client();
    second.send("GET /second HTTP/1.1\r\nHost: t\r\n\r\n");
    second
        .stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let err = second.stream.read(&mut [0u8; 64]).unwrap_err();
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "{err}"
    );

    // Once the first connection closes, the second one is served.
    drop(first);
    second
        .stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let r = second.reply();
    assert!(r.text().starts_with("GET /second "), "{}", r.text());

    // The slot is reused again and again.
    drop(second);
    for i in 0..20 {
        let mut c = server.client();
        c.send(format!(
            "GET /{i} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
        ));
        assert_eq!(c.reply().status, 200);
        c.expect_closed();
    }
}

#[cfg(unix)]
#[test]
fn at_the_connection_limit_a_connection_mid_request_keeps_its_place() {
    let server = TestServer::start(Server::new().max_connections(1), echo);
    let mut first = server.client();
    // Half a request: the connection is busy, not idle, once the server
    // has read it.
    first.send("GET /first HTTP/1.1\r\n");
    std::thread::sleep(Duration::from_millis(200));

    // The kernel completes the second handshake, but the server does not
    // accept the connection while the first one is receiving a request.
    let mut second = server.client();
    second.send("GET /second HTTP/1.1\r\nHost: t\r\n\r\n");
    second
        .stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let err = second.stream.read(&mut [0u8; 64]).unwrap_err();
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "{err}"
    );

    // The first request is answered in full. Idle after it, the first
    // connection gives its place to the waiting one: it is closed cleanly,
    // and the second is served.
    first.send("Host: t\r\n\r\n");
    let r = first.reply();
    assert!(r.text().starts_with("GET /first "), "{}", r.text());
    first.expect_closed();
    second
        .stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let r = second.reply();
    assert!(r.text().starts_with("GET /second "), "{}", r.text());

    // The slot is reused again and again.
    drop(second);
    for i in 0..20 {
        let mut c = server.client();
        c.send(format!(
            "GET /{i} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n"
        ));
        assert_eq!(c.reply().status, 200);
        c.expect_closed();
    }
}

/// At the limit, idle keep-alive connections do not keep new clients out:
/// the one idle longest is closed to make room, as nginx does.
#[cfg(unix)]
#[test]
fn at_the_connection_limit_the_longest_idle_connection_makes_room() {
    let server = TestServer::start(Server::new().max_connections(3), echo);
    let mut idle: Vec<Client> = (0..3)
        .map(|i| {
            let mut c = server.client();
            c.send(format!("GET /{i} HTTP/1.1\r\nHost: t\r\n\r\n"));
            assert_eq!(c.reply().status, 200);
            c
        })
        .collect();
    // The oldest becomes the newest idle connection again.
    idle[0].send("GET /again HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(idle[0].reply().status, 200);

    // Two new clients: connections 1 and 2, idle longest, close for them.
    let mut fresh: Vec<Client> = (0..2)
        .map(|i| {
            let mut c = server.client();
            c.send(format!("GET /new{i} HTTP/1.1\r\nHost: t\r\n\r\n"));
            let r = c.reply();
            assert!(
                r.text().starts_with(&format!("GET /new{i} ")),
                "{}",
                r.text()
            );
            c
        })
        .collect();
    idle[1].expect_closed();
    idle[2].expect_closed();
    // The others carry on.
    idle[0].send("GET /still HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(idle[0].reply().text().starts_with("GET /still "));
    for c in &mut fresh {
        c.send("GET /more HTTP/1.1\r\nHost: t\r\n\r\n");
        assert!(c.reply().text().starts_with("GET /more "));
    }
}

/// An idle HTTP/2 connection gives its place the same way, with a GOAWAY.
#[cfg(unix)]
#[test]
fn at_the_connection_limit_an_idle_http2_connection_goes_away() {
    let server = TestServer::start(Server::new().max_connections(1), echo);
    let mut h2 = server.client();
    // The preface and an empty SETTINGS frame: a connection with no stream.
    h2.send(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0");
    // Wait for the server's SETTINGS: the connection is open and idle.
    while h2.buf.len() < 9 {
        assert!(h2.fill());
    }

    let mut c = server.client();
    c.send("GET /new HTTP/1.1\r\nHost: t\r\n\r\n");
    assert!(c.reply().text().starts_with("GET /new "));

    // The HTTP/2 connection got a GOAWAY, then was closed.
    while h2.fill() {}
    let mut frames = Vec::new();
    let mut at = 0;
    while at + 9 <= h2.buf.len() {
        let len = u32::from_be_bytes([0, h2.buf[at], h2.buf[at + 1], h2.buf[at + 2]]) as usize;
        frames.push(h2.buf[at + 3]);
        at += 9 + len;
    }
    assert_eq!(frames.last(), Some(&7), "frame types {frames:?}");
}

/// Below `min_free_memory`, new connections are refused with a `503` that
/// names the code, and closed. A reserve larger than any machine's memory
/// stands in for a process near its memory limit (`scripts/idle-connections.sh`
/// runs the real thing in a container in CI); `0` turns the check off.
#[test]
fn new_connections_are_refused_while_memory_is_low() {
    let server = TestServer::start(Server::new().min_free_memory(usize::MAX), echo);
    for _ in 0..3 {
        let mut c = server.client();
        c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        let r = c.reply();
        assert_eq!(r.status, 503);
        assert_eq!(r.header("retry-after"), Some("1"));
        assert_eq!(r.header("connection"), Some("close"));
        assert!(r.text().starts_with("GIONG-E1121"), "{}", r.text());
        c.expect_closed();
    }

    let server = TestServer::start(Server::new().min_free_memory(0), echo);
    let mut c = server.client();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().status, 200);
}

#[test]
fn file_bodies_are_copied_over_a_unix_socket() {
    use std::os::unix::net::UnixStream;

    let data = file_data(700_001);
    let tmp = TempFile::new("file-unix", &data);
    let path = std::env::temp_dir().join(format!("giong-http-{}-file.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let (tx, rx) = mpsc::channel();
    let (server_path, handler) = (path.clone(), file_handler(tmp.0.clone()));
    let thread = std::thread::spawn(move || {
        rt.run(move |_| {
            let (tx, path, handler) = (tx.clone(), server_path.clone(), handler.clone());
            async move {
                let listener = giong::net::UnixListener::bind(path).unwrap();
                tx.send(()).unwrap();
                Server::new().serve_unix(listener, handler).await;
            }
        })
        .unwrap();
    });
    rx.recv().unwrap();
    let mut stream = UnixStream::connect(&path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET /file HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut all = Vec::new();
    stream.read_to_end(&mut all).unwrap();
    let end = all.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&all[..end]).into_owned();
    assert!(head.contains("Content-Length: 700001\r\n"), "{head}");
    assert!(all[end..] == data[..]);
    handle.shutdown();
    thread.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

thread_local! {
    /// Requests the `/hoard` route keeps until `/release`.
    static HOARD: RefCell<Vec<Request>> = const { RefCell::new(Vec::new()) };
}

/// Echoes the body's length; `/hoard` keeps the request, `/release` drops
/// the kept ones.
async fn budgeted(req: Request) -> Response {
    let len = req.body().len();
    match req.path() {
        "/hoard" => HOARD.with(|kept| kept.borrow_mut().push(req)),
        "/release" => HOARD.with(|kept| kept.borrow_mut().clear()),
        _ => {}
    }
    Response::text(format!("got {len}"))
}

/// Sends the head of a `POST` of `len` bytes with `Expect: 100-continue`
/// and returns the first answer: `100` if the server would take the body.
fn probe(server: &TestServer, len: usize) -> Reply {
    let mut c = server.client();
    c.send(format!(
        "POST / HTTP/1.1\r\nHost: t\r\nExpect: 100-continue\r\nContent-Length: {len}\r\n\r\n"
    ));
    c.reply()
}

/// Probes until the server refuses a body of `len` bytes: the bytes sent
/// before have arrived and been counted.
fn wait_until_refused(server: &TestServer, len: usize) -> Reply {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let reply = probe(server, len);
        if reply.status != 100 {
            return reply;
        }
        assert!(Instant::now() < until, "a {len}-byte body is still taken");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn requests_being_received_share_a_budget_per_worker() {
    let server = TestServer::start(
        Server::new().max_buffered_request_bytes(64 * 1024),
        budgeted,
    );

    // A slow upload holds 40,000 bytes of its 50,000.
    let mut slow = server.client();
    slow.send("POST /a HTTP/1.1\r\nHost: t\r\nContent-Length: 50000\r\n\r\n");
    slow.send(vec![b'a'; 40_000]);

    // Another 40,000 bytes do not fit next to it: refused before the body
    // is read, with a code and a time to retry.
    let refused = wait_until_refused(&server, 40_000);
    assert_eq!(refused.status, 503);
    assert_eq!(refused.header("retry-after"), Some("1"));
    assert_eq!(refused.header("connection"), Some("close"));
    assert!(
        refused.text().starts_with("GIONG-E1120"),
        "{}",
        refused.text()
    );

    // Small requests still get through, and so does a body that fits.
    let mut c = server.client();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().text(), "got 0");
    c.send(format!(
        "POST / HTTP/1.1\r\nHost: t\r\nContent-Length: 20000\r\n\r\n{}",
        "b".repeat(20_000)
    ));
    assert_eq!(c.reply().text(), "got 20000");

    // A body whose length is not announced is refused once it grows past
    // what is left; chunked, the decoded copy counts too.
    let mut chunked = server.client();
    chunked.send("POST / HTTP/1.1\r\nHost: t\r\nTransfer-Encoding: chunked\r\n\r\n");
    for _ in 0..10 {
        if chunked
            .stream
            .write_all(format!("1000\r\n{}\r\n", "c".repeat(4096)).as_bytes())
            .is_err()
        {
            break;
        }
    }
    let cut = chunked.reply();
    assert_eq!(cut.status, 503);
    assert!(cut.text().starts_with("GIONG-E1120"));

    // The slow upload finishes; its bytes go back once it is answered, and
    // the refused upload now fits.
    slow.send(vec![b'a'; 10_000]);
    assert_eq!(slow.reply().text(), "got 50000");
    assert_eq!(probe(&server, 40_000).status, 100);

    // Larger than the whole budget: no retry can help.
    assert_eq!(probe(&server, 70_000).status, 413);
}

#[test]
fn a_request_kept_by_its_handler_keeps_its_share_of_the_budget() {
    let server = TestServer::start(
        Server::new().max_buffered_request_bytes(64 * 1024),
        budgeted,
    );
    let mut c = server.client();
    c.send(format!(
        "POST /hoard HTTP/1.1\r\nHost: t\r\nContent-Length: 50000\r\n\r\n{}",
        "h".repeat(50_000)
    ));
    assert_eq!(c.reply().text(), "got 50000");
    // The handler still holds those bytes.
    assert_eq!(probe(&server, 30_000).status, 503);
    c.send("GET /release HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().text(), "got 0");
    assert_eq!(probe(&server, 30_000).status, 100);
}

#[test]
fn the_budget_shows_in_debug_and_defaults_to_auto() {
    let text = format!("{:?}", Server::new());
    assert!(text.contains("max_buffered_request_bytes: auto"), "{text}");
    let text = format!("{:?}", Server::new().max_buffered_request_bytes(1234));
    assert!(text.contains("max_buffered_request_bytes: 1234"), "{text}");
}

/// Answers with the body's length and a checksum that notices any byte out
/// of place.
async fn checksum(req: Request) -> Response {
    Response::text(checksum_of(req.body()))
}

fn checksum_of(body: &[u8]) -> String {
    let sum = body.iter().enumerate().fold(0u64, |a, (i, &b)| {
        a.wrapping_mul(31).wrapping_add(u64::from(b) ^ i as u64)
    });
    format!("{} {sum}", body.len())
}

/// Bodies from 64 KiB up are received straight into the request buffer
/// (`Transport::recv_into`). Large and small requests in turn on one
/// connection, some pipelined behind a large body, some sent in pieces or
/// after `100 Continue`: each body arrives whole and in order.
#[test]
fn large_bodies_arrive_whole_between_other_requests() {
    let server = TestServer::start(Server::new().max_body_size(4 << 20), checksum);
    let mut c = server.client();
    let body = |len: usize, seed: u8| -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
            .collect()
    };
    let post = |len: usize| format!("POST /u HTTP/1.1\r\nHost: t\r\nContent-Length: {len}\r\n\r\n");
    for (round, &len) in [70_000usize, 10, 1 << 20, 65_536, 300_000, 3 << 20]
        .iter()
        .enumerate()
    {
        let b = body(len, round as u8);
        // A large body, with a small request pipelined right behind it.
        let mut bytes = post(len).into_bytes();
        bytes.extend_from_slice(&b);
        bytes.extend_from_slice(b"GET /after HTTP/1.1\r\nHost: t\r\n\r\n");
        c.send(&bytes);
        let r = c.reply();
        assert_eq!(
            (r.status, r.text()),
            (200, checksum_of(&b).as_str()),
            "round {round}"
        );
        let r = c.reply();
        assert_eq!((r.status, r.text()), (200, "0 0"), "round {round}");
        // The same body in pieces, with pauses between them.
        c.send(post(len));
        for piece in b.chunks(len / 3 + 1) {
            std::thread::sleep(Duration::from_millis(5));
            c.send(piece);
        }
        let r = c.reply();
        assert_eq!(
            (r.status, r.text()),
            (200, checksum_of(&b).as_str()),
            "round {round}"
        );
    }
    // After `100 Continue`.
    let b = body(500_000, 9);
    c.send("POST /u HTTP/1.1\r\nHost: t\r\nContent-Length: 500000\r\nExpect: 100-continue\r\n\r\n");
    while !c.buf.ends_with(b"\r\n\r\n") {
        assert!(c.fill());
    }
    assert_eq!(c.buf, b"HTTP/1.1 100 Continue\r\n\r\n");
    c.buf.clear();
    c.send(&b);
    let r = c.reply();
    assert_eq!((r.status, r.text()), (200, checksum_of(&b).as_str()));
}

/// Bodies of 64 KiB and more are received straight into the request buffer,
/// and charged against the worker's budget as they arrive, like any other:
/// two uploads admitted together cannot both hold more than the budget.
#[test]
fn large_bodies_received_straight_into_the_buffer_are_charged_as_they_arrive() {
    let server = TestServer::start(
        Server::new().max_buffered_request_bytes(256 * 1024),
        budgeted,
    );
    let head = "POST /big HTTP/1.1\r\nHost: t\r\nContent-Length: 200000\r\n\r\n";
    // Both are admitted while nothing is held yet.
    let mut a = server.client();
    let mut b = server.client();
    a.send(head);
    b.send(head);
    std::thread::sleep(Duration::from_millis(100));

    // The first holds 150,000 bytes: a probe for as much again is refused.
    a.send(vec![b'a'; 150_000]);
    let refused = wait_until_refused(&server, 150_000);
    assert_eq!(refused.status, 503);

    // The second sends as much: it does not fit next to the first, and is
    // refused once its bytes arrive.
    let _ = b.stream.write_all(&vec![b'b'; 150_000]);
    let cut = b.reply();
    assert_eq!(cut.status, 503);
    assert!(cut.text().starts_with("GIONG-E1120"), "{}", cut.text());

    // The first completes.
    a.send(vec![b'a'; 50_000]);
    assert_eq!(a.reply().text(), "got 200000");
}

/// The write timeout counts from the last progress of a write, not from its
/// start: a large answer to a client that keeps reading, slowly, goes out
/// whole even when that takes far longer than the timeout. A client that
/// stops reading loses the connection one timeout after the last progress.
#[test]
fn the_write_timeout_counts_from_the_last_progress() {
    const LEN: usize = 32 << 20;
    async fn large(_req: Request) -> Response {
        Response::new(200).body(vec![b'x'; LEN])
    }
    let server = TestServer::start(Server::new().write_timeout(Duration::from_secs(1)), large);
    // Read the whole answer: the head, then `LEN` bytes of body.
    let read_all = |pace: Option<Duration>| {
        let mut c = server.client();
        c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
        let start = Instant::now();
        let mut got = 0usize;
        let mut buf = vec![0u8; 256 * 1024];
        let mut paced = 0;
        loop {
            match c.stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => got += n,
            }
            // A pause after each 256 KiB.
            if let Some(pace) = pace {
                if got - paced >= buf.len() {
                    paced = got;
                    std::thread::sleep(pace);
                }
            }
            if got > LEN {
                break;
            }
        }
        (got, start.elapsed())
    };
    // About 10 MB a second: some seconds for the part that does not fit in
    // the sockets' buffers, well past the 1 s timeout. The timeout leaves
    // room for a slow CI machine that pauses the reader now and then (a
    // 300 ms one cut the answer on a macOS runner).
    let (got, took) = read_all(Some(Duration::from_millis(25)));
    assert!(
        got > LEN,
        "the answer was cut after {got} bytes, in {took:?}"
    );
    assert!(took > Duration::from_secs(2), "{took:?}");

    // A client that takes nothing more after its first read is dropped.
    let mut c = server.client();
    c.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = c.stream.read(&mut buf).unwrap();
    std::thread::sleep(Duration::from_secs(3));
    loop {
        match c.stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got += n,
        }
    }
    assert!(got < LEN, "a stalled client got the whole answer");
}
