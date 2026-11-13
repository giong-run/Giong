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
use giong_http::{Request, Response, Server};

// ------------------------------------------------------------------ harness

struct TestServer {
    addr: SocketAddr,
    handle: giong::Handle,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start<H, Fut>(server: Server, handler: H) -> Self
    where
        H: Fn(Request) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Response> + 'static,
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
        "{} {} q={:?} v=1.{} ua={:?} body={}",
        req.method(),
        req.path(),
        req.query(),
        req.version_minor(),
        req.header_str("user-agent"),
        String::from_utf8_lossy(req.body()),
    );
    Response::text(text).header("X-Body-Len", &req.body().len().to_string())
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

    // A slow drip does not reset the clock.
    let mut c = server.client();
    let start = Instant::now();
    c.send("GET / HTTP/1.1\r\n");
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(20));
        if c.stream.write_all(b"X-Drip: 1\r\n").is_err() {
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
    let mut c = server.client();
    for i in 0..4 {
        std::thread::sleep(Duration::from_millis(150));
        c.send(format!("GET /{i} HTTP/1.1\r\nHost: t\r\n\r\n"));
        assert_eq!(c.reply().status, 200);
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
fn a_panicking_handler_only_takes_down_its_own_connection() {
    async fn handler(req: Request) -> Response {
        if req.path() == "/panic" {
            panic!("handler panic (expected by this test)");
        }
        Response::text("fine")
    }
    let server = TestServer::start(Server::new(), handler);
    let mut other = server.client();
    other.send("GET / HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(other.reply().text(), "fine");

    let mut c = server.client();
    c.send("GET /panic HTTP/1.1\r\nHost: t\r\n\r\n");
    c.expect_closed();

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
    busy.send("GET /slow HTTP/1.1\r\nHost: t\r\n\r\n");
    std::thread::sleep(Duration::from_millis(100));

    let stopped_after = server.stop();

    // The request in progress was answered, and told to go away.
    let r = busy.reply();
    assert_eq!((r.status, r.text()), (200, "done"));
    assert_eq!(r.header("connection"), Some("close"));
    busy.expect_closed();
    idle.expect_closed();
    // The runtime waited for the slow request, not for the 20 s grace period.
    assert!(
        stopped_after >= Duration::from_millis(250),
        "{stopped_after:?}"
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
