//! Streaming bodies, end to end: request bodies read as they arrive
//! (`Handler::stream_body`) and response bodies written as they are
//! produced (`Body::channel`), on a real socket, driven by a blocking
//! client that speaks raw HTTP.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong::time::sleep;
use giong_http::{Body, Handler, Request, Response, Server};

// ------------------------------------------------------------------ harness

struct TestServer {
    addr: SocketAddr,
    handle: giong::Handle,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(server: Server) -> Self {
        let rt = giong::Builder::new()
            .workers(1)
            .shutdown_grace(Duration::from_secs(5))
            .build()
            .unwrap();
        let handle = rt.handle();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            rt.run(move |_| {
                let (tx, server) = (tx.clone(), server.clone());
                async move {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    server.serve(listener, App).await;
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
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.shutdown();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
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

    fn head(&mut self) -> Reply {
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            assert!(self.fill(), "connection closed before a full response head");
        };
        let head = String::from_utf8(self.buf.drain(..end).collect()).unwrap();
        let status = head.split(' ').nth(1).unwrap().parse().unwrap();
        Reply {
            status,
            head,
            body: Vec::new(),
        }
    }

    fn line(&mut self) -> String {
        let end = loop {
            if let Some(i) = self.buf.windows(2).position(|w| w == b"\r\n") {
                break i;
            }
            assert!(
                self.fill(),
                "connection closed in the middle of a chunked body"
            );
        };
        let line = String::from_utf8(self.buf[..end].to_vec()).unwrap();
        self.buf.drain(..end + 2);
        line
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        while self.buf.len() < n {
            assert!(self.fill(), "connection closed in the middle of a body");
        }
        self.buf.drain(..n).collect()
    }

    /// One chunk of a chunked body; `None` at the last one (trailers read).
    fn chunk(&mut self) -> Option<Vec<u8>> {
        let size = usize::from_str_radix(&self.line(), 16).unwrap();
        if size == 0 {
            assert_eq!(self.line(), "", "trailers");
            return None;
        }
        let data = self.take(size);
        assert_eq!(self.take(2), b"\r\n");
        Some(data)
    }

    /// Reads one response, whatever its framing. `head_only`: the response
    /// to a HEAD request.
    fn reply_to(&mut self, head_only: bool) -> Reply {
        let mut reply = self.head();
        if head_only || matches!(reply.status, 100 | 204 | 304) {
            return reply;
        }
        if reply.header("transfer-encoding") == Some("chunked") {
            while let Some(data) = self.chunk() {
                reply.body.extend_from_slice(&data);
            }
        } else if let Some(len) = reply.header("content-length") {
            reply.body = self.take(len.parse().unwrap());
        } else {
            while self.fill() {}
            reply.body = std::mem::take(&mut self.buf);
        }
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

/// What the handlers saw, by the test's tag (`X-Tag`): tests run at the
/// same time, in one process.
static SEEN: Mutex<Option<HashMap<String, Vec<String>>>> = Mutex::new(None);
/// Tags whose `/events` producer may send its second event.
static RELEASED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

fn record(tag: &str, what: impl Into<String>) {
    let mut seen = SEEN.lock().unwrap();
    let map = seen.get_or_insert_with(HashMap::new);
    map.entry(tag.to_string()).or_default().push(what.into());
}

/// Waits until a handler has recorded something under `tag`.
fn seen(tag: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let found = SEEN
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|map| map.get(tag).cloned());
        if let Some(found) = found {
            return found;
        }
        assert!(Instant::now() < deadline, "nothing recorded for {tag}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn release(tag: &str) {
    let mut released = RELEASED.lock().unwrap();
    released
        .get_or_insert_with(HashSet::new)
        .insert(tag.to_string());
}

fn is_released(tag: &str) -> bool {
    let released = RELEASED.lock().unwrap();
    released.as_ref().is_some_and(|set| set.contains(tag))
}

/// FNV-1a, to compare large bodies in a short answer.
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// The streamed pieces `/download` sends: small ones, and large ones the
/// server writes without copying.
const PIECES: &[usize] = &[1, 10, 40_000, 3, 70_000, 0, 5];

/// The largest body a streaming route accepts.
const LIMIT: u64 = 300_000;

struct App;

impl Handler for App {
    fn stream_body(&self, req: &Request) -> Option<u64> {
        assert!(req.body().is_empty() && req.body_stream().is_none());
        (req.path() != "/whole").then_some(LIMIT)
    }

    async fn call(&self, req: Request) -> Response {
        let tag = req.header_str("x-tag").unwrap_or_default().to_string();
        match req.path() {
            "/whole" => {
                assert!(req.body_stream().is_none());
                Response::text(format!("whole {}", String::from_utf8_lossy(req.body())))
            }
            "/upload" => {
                let body = req.body_stream().expect("a streamed body");
                assert!(req.body().is_empty());
                let mut all = Vec::new();
                loop {
                    match body.chunk().await {
                        Ok(Some(piece)) => {
                            assert!(!piece.is_empty());
                            all.extend_from_slice(&piece);
                        }
                        Ok(None) => break,
                        Err(e) => {
                            record(&tag, e.to_string());
                            return Response::new(400).body(e.to_string());
                        }
                    }
                }
                // The end stays the end.
                assert!(matches!(body.chunk().await, Ok(None)));
                assert_eq!(body.bytes_read(), all.len() as u64);
                let length = body
                    .content_length()
                    .map_or("chunked".into(), |n| n.to_string());
                Response::text(format!("{} {:016x} {length}", all.len(), fnv(&all)))
            }
            "/refuse" => Response::new(401),
            "/half" => {
                let body = req.body_stream().unwrap();
                let first = body.chunk().await.unwrap().unwrap();
                // A clone kept past the answer finds the body abandoned.
                let kept = body.clone();
                drop(giong::spawn(async move {
                    sleep(Duration::from_millis(50)).await;
                    loop {
                        match kept.chunk().await {
                            Ok(Some(_)) => continue,
                            Ok(None) => break record(&tag, "end"),
                            Err(e) => break record(&tag, e.to_string()),
                        }
                    }
                }));
                Response::text(format!("got {}", first.len()))
            }
            "/download" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    let all = pattern(PIECES.iter().sum());
                    let mut at = 0;
                    for &n in PIECES {
                        if let Err(e) = tx.send(all[at..at + n].to_vec()).await {
                            return record(&tag, e.to_string());
                        }
                        at += n;
                    }
                    record(&tag, "sent");
                }));
                Response::new(200)
                    .header("Content-Type", "application/octet-stream")
                    .body(body)
            }
            "/events" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    tx.send("data: one\n\n").await.unwrap();
                    // The second event only exists once the client has the
                    // first one.
                    while !is_released(&tag) {
                        sleep(Duration::from_millis(2)).await;
                    }
                    tx.send("data: two\n\n").await.unwrap();
                }));
                Response::new(200)
                    .header("Content-Type", "text/event-stream")
                    .body(body)
            }
            "/abort" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    tx.send("partial").await.unwrap();
                    tx.abort();
                }));
                Response::new(200).body(body)
            }
            "/forever" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    loop {
                        if let Err(e) = tx.send(vec![b'x'; 1024]).await {
                            assert!(tx.is_closed());
                            return record(&tag, e.to_string());
                        }
                        sleep(Duration::from_millis(1)).await;
                    }
                }));
                Response::new(200).body(body)
            }
            "/no-content" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    match tx.send("never").await {
                        Ok(()) => record(&tag, "sent"),
                        Err(e) => record(&tag, e.to_string()),
                    }
                }));
                Response::new(204).body(body)
            }
            _ => Response::new(404),
        }
    }
}

fn server() -> TestServer {
    // A tiny `max_body_size`: streamed bodies are not bound by it.
    TestServer::start(Server::new().max_body_size(16))
}

fn upload(tag: &str, len: usize) -> String {
    format!("POST /upload HTTP/1.1\r\nHost: t\r\nX-Tag: {tag}\r\nContent-Length: {len}\r\n\r\n")
}

fn chunked_upload(tag: &str) -> String {
    format!(
        "POST /upload HTTP/1.1\r\nHost: t\r\nX-Tag: {tag}\r\nTransfer-Encoding: chunked\r\n\r\n"
    )
}

// ------------------------------------------------------------ request bodies

#[test]
fn a_large_body_arrives_piece_by_piece_and_the_connection_stays_usable() {
    let server = server();
    let mut c = server.client();
    let body = pattern(250_000);
    c.send(upload("large", body.len()));
    for piece in body.chunks(7_000) {
        c.send(piece);
    }
    let r = c.reply();
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.text(), format!("250000 {:016x} 250000", fnv(&body)));
    assert_eq!(r.header("connection"), None);

    // A request right behind the body, in the same write.
    let mut both = upload("behind", 5).into_bytes();
    both.extend_from_slice(b"hello");
    both.extend_from_slice(b"POST /whole HTTP/1.1\r\nHost: t\r\nContent-Length: 3\r\n\r\nabc");
    c.send(both);
    assert_eq!(c.reply().text(), format!("5 {:016x} 5", fnv(b"hello")));
    assert_eq!(c.reply().text(), "whole abc");
}

#[test]
fn a_chunked_body_is_decoded_as_it_arrives() {
    let server = server();
    let mut c = server.client();
    let mut raw = chunked_upload("chunked").into_bytes();
    raw.extend_from_slice(b"5;ext=1\r\nhello\r\n1\r\n \r\n");
    let big = pattern(100_000);
    raw.extend_from_slice(format!("{:x}\r\n", big.len()).as_bytes());
    raw.extend_from_slice(&big);
    raw.extend_from_slice(b"\r\n0\r\nX-Trailer: 1\r\n\r\n");
    raw.extend_from_slice(b"GET /whole HTTP/1.1\r\nHost: t\r\n\r\n");
    // Sent in uneven pieces, splitting the framing anywhere.
    for piece in raw.chunks(4_093) {
        c.send(piece);
    }
    let mut expected = b"hello ".to_vec();
    expected.extend_from_slice(&big);
    let r = c.reply();
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.text(),
        format!("{} {:016x} chunked", expected.len(), fnv(&expected))
    );
    assert_eq!(c.reply().text(), "whole ");
}

#[test]
fn bodies_past_the_handlers_limit_are_refused() {
    let server = server();

    // Announced too long: 413 without calling the handler.
    let mut c = server.client();
    c.send(upload("announced", LIMIT as usize + 1));
    let r = c.reply();
    assert_eq!(r.status, 413);
    c.expect_closed();

    // Chunked and growing too long: the handler's stream fails.
    let mut c = server.client();
    c.send(chunked_upload("grown"));
    c.send(b"10\r\n0123456789abcdef\r\n");
    c.send(format!("{:x}\r\n", LIMIT).as_bytes());
    let r = c.reply();
    assert_eq!(r.status, 400);
    assert!(r.text().starts_with("GIONG-E1111"), "{}", r.text());
    assert_eq!(r.header("connection"), Some("close"));
    assert!(seen("grown")[0].starts_with("GIONG-E1111"));
}

#[test]
fn broken_chunked_framing_fails_the_stream() {
    let server = server();
    let mut c = server.client();
    c.send(chunked_upload("framing"));
    c.send(b"3\r\nabcX\r\n");
    let r = c.reply();
    assert_eq!(r.status, 400);
    assert!(r.text().starts_with("GIONG-E1113"), "{}", r.text());
    assert_eq!(r.header("connection"), Some("close"));
}

#[test]
fn nothing_is_read_and_nothing_continues_until_the_handler_asks() {
    let server = server();

    // Asked: `100 Continue`, then the body.
    let mut c = server.client();
    let head = upload("continue", 5).replace("\r\n\r\n", "\r\nExpect: 100-continue\r\n\r\n");
    c.send(head);
    assert_eq!(c.reply().status, 100);
    c.send(b"hello");
    let r = c.reply();
    assert_eq!(r.text(), format!("5 {:016x} 5", fnv(b"hello")));

    // Refused before asking: no `100 Continue`, and the connection closes
    // since the body was never read.
    let mut c = server.client();
    let head = "POST /refuse HTTP/1.1\r\nHost: t\r\nExpect: 100-continue\r\nContent-Length: 200000\r\n\r\n";
    c.send(head);
    let r = c.reply();
    assert_eq!(r.status, 401);
    assert_eq!(r.header("connection"), Some("close"));
    c.expect_closed();
}

#[test]
fn answering_before_the_end_of_the_body_closes_the_connection() {
    let server = server();
    let mut c = server.client();
    c.send("POST /half HTTP/1.1\r\nHost: t\r\nX-Tag: half\r\nContent-Length: 100000\r\n\r\n");
    c.send(&pattern(10)[..]);
    let r = c.reply();
    assert_eq!(r.text(), "got 10");
    assert_eq!(r.header("connection"), Some("close"));
    // What the client still sends is read and dropped, then the connection
    // closes.
    let _ = c.stream.write_all(&pattern(20_000));
    c.expect_closed();
    assert!(
        seen("half")[0].starts_with("GIONG-E1112"),
        "{:?}",
        seen("half")
    );
}

#[test]
fn a_body_cut_short_fails_the_stream() {
    let server = server();
    let mut c = server.client();
    c.send(upload("cut", 1000));
    c.send(&pattern(100)[..]);
    c.stream.shutdown(std::net::Shutdown::Write).unwrap();
    assert!(
        seen("cut")[0].starts_with("GIONG-E1112"),
        "{:?}",
        seen("cut")
    );
}

#[test]
fn each_piece_has_the_request_timeout_but_the_whole_body_has_none() {
    let server = TestServer::start(Server::new().request_timeout(Duration::from_millis(300)));

    // Ten pieces 100 ms apart: a second in all, well past the timeout.
    let mut c = server.client();
    c.send(upload("steady", 100));
    for piece in pattern(100).chunks(10) {
        std::thread::sleep(Duration::from_millis(100));
        c.send(piece);
    }
    let r = c.reply();
    assert_eq!(r.status, 200, "{}", r.text());

    // Half the body, then nothing.
    let mut c = server.client();
    c.send(upload("stalled", 100));
    c.send(&pattern(50)[..]);
    let started = Instant::now();
    let r = c.reply();
    assert_eq!(r.status, 400);
    assert!(r.text().starts_with("GIONG-E1112"), "{}", r.text());
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(r.header("connection"), Some("close"));
}

// ----------------------------------------------------------- response bodies

#[test]
fn a_streamed_response_is_chunked_and_the_connection_stays_usable() {
    let server = server();
    let mut c = server.client();
    c.send("GET /download HTTP/1.1\r\nHost: t\r\nX-Tag: download\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.status, 200);
    assert_eq!(r.header("transfer-encoding"), Some("chunked"));
    assert_eq!(r.header("content-length"), None);
    assert_eq!(r.body, pattern(PIECES.iter().sum()));
    assert_eq!(seen("download"), ["sent"]);

    c.send("GET /whole HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().text(), "whole ");
}

#[test]
fn each_piece_goes_out_as_soon_as_it_is_sent() {
    let server = server();
    let mut c = server.client();
    c.send("GET /events HTTP/1.1\r\nHost: t\r\nX-Tag: events\r\n\r\n");
    let r = c.head();
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
    // The producer is waiting for the test before it sends the second.
    assert_eq!(c.chunk().unwrap(), b"data: one\n\n");
    release("events");
    assert_eq!(c.chunk().unwrap(), b"data: two\n\n");
    assert_eq!(c.chunk(), None);
}

#[test]
fn an_http_1_0_client_gets_the_body_until_the_connection_closes() {
    let server = server();
    let mut c = server.client();
    c.send("GET /download HTTP/1.0\r\nConnection: keep-alive\r\nX-Tag: old\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.header("transfer-encoding"), None);
    assert_eq!(r.header("connection"), Some("close"));
    assert_eq!(r.body, pattern(PIECES.iter().sum()));
}

#[test]
fn responses_without_a_body_tell_the_producer() {
    let server = server();
    let mut c = server.client();
    c.send("HEAD /download HTTP/1.1\r\nHost: t\r\nX-Tag: head\r\n\r\n");
    let r = c.reply_to(true);
    assert_eq!(r.status, 200);
    assert!(
        seen("head")[0].starts_with("GIONG-E1114"),
        "{:?}",
        seen("head")
    );

    c.send("GET /no-content HTTP/1.1\r\nHost: t\r\nX-Tag: 204\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.status, 204);
    assert_eq!(r.header("transfer-encoding"), None);
    assert!(
        seen("204")[0].starts_with("GIONG-E1114"),
        "{:?}",
        seen("204")
    );

    c.send("GET /whole HTTP/1.1\r\nHost: t\r\n\r\n");
    assert_eq!(c.reply().text(), "whole ");
}

#[test]
fn an_aborted_body_ends_without_its_last_chunk() {
    let server = server();
    let mut c = server.client();
    c.send("GET /abort HTTP/1.1\r\nHost: t\r\n\r\n");
    let r = c.head();
    assert_eq!(r.status, 200);
    assert_eq!(c.chunk().unwrap(), b"partial");
    // The connection closes where the next chunk size would be.
    c.expect_closed();
}

#[test]
fn a_client_that_goes_away_stops_the_producer() {
    let server = server();
    let mut c = server.client();
    c.send("GET /forever HTTP/1.1\r\nHost: t\r\nX-Tag: gone\r\n\r\n");
    let _ = c.head();
    let _ = c.chunk().unwrap();
    drop(c);
    assert!(
        seen("gone")[0].starts_with("GIONG-E1114"),
        "{:?}",
        seen("gone")
    );
}
