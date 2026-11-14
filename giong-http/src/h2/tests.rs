//! End to end: a real server on a real socket, driven by a blocking client
//! that writes its frames by hand, so that it can also misbehave.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong::time::sleep;

use super::frame::{self, Head, Reason, PREFACE};
use super::hpack::{Decoder, Encoder, Index, TABLE_SIZE};
use crate::{Body, Handler, Request, Response, Server};

// ------------------------------------------------------------------ server

#[derive(Default)]
struct State {
    /// Lets `/wait` handlers answer.
    released: AtomicBool,
    /// Handlers started.
    started: AtomicUsize,
    /// Handlers that have answered.
    answered: AtomicUsize,
    log: Mutex<Vec<String>>,
}

impl State {
    fn record(&self, what: impl Into<String>) {
        self.log.lock().unwrap().push(what.into());
    }

    fn logged(&self, what: &str) -> bool {
        self.log.lock().unwrap().iter().any(|l| l == what)
    }

    /// Waits up to a second for `what` to be logged.
    fn wait_for(&self, what: &str) -> bool {
        let until = Instant::now() + Duration::from_secs(1);
        while Instant::now() < until {
            if self.logged(what) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }
}

/// How long a client waits for the server before the test fails: long,
/// because a panic's backtrace under ThreadSanitizer takes seconds.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Waits for `done`, for as long as a slow run (a sanitizer) may need.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let until = Instant::now() + READ_TIMEOUT;
    while !done() {
        assert!(Instant::now() < until, "still waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Logs when a handler is dropped.
struct Guard(Arc<State>, String);

impl Drop for Guard {
    fn drop(&mut self) {
        self.0.record(format!("dropped {}", self.1));
    }
}

#[derive(Clone, Default)]
struct App(Arc<State>);

fn query_number(req: &Request, key: &str) -> usize {
    req.query()
        .and_then(|q| {
            q.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                (k == key).then(|| v.parse().ok()).flatten()
            })
        })
        .unwrap_or(0)
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

/// The file the `/file` paths serve, written by the test that asks for
/// them.
fn file_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("giong-http-h2-{}", std::process::id()))
}

impl Handler for App {
    fn stream_body(&self, req: &Request) -> Option<u64> {
        if req.path() == "/panic-deciding" {
            panic!("stream_body panic (expected by this test)");
        }
        req.path().starts_with("/pipe").then_some(4 << 20)
    }

    async fn call(&self, req: Request) -> Response {
        let state = &self.0;
        state.started.fetch_add(1, Ordering::SeqCst);
        let resp = self.answer(req).await;
        state.answered.fetch_add(1, Ordering::SeqCst);
        resp
    }
}

impl App {
    async fn answer(&self, req: Request) -> Response {
        let state = &self.0;
        match req.path() {
            "/" => Response::text("hello"),
            "/echo" => Response::new(200).body(req.body().to_vec()),
            "/info" => {
                let cookie = req.header_str("cookie").unwrap_or("-");
                Response::text(format!(
                    "{} {} {:?} {} host={} cookie={} x-a={}",
                    req.method(),
                    req.path(),
                    req.query(),
                    req.version(),
                    req.header_str("host").unwrap_or("-"),
                    cookie,
                    req.header_str("x-a").unwrap_or("-"),
                ))
            }
            "/bytes" => Response::new(200).body(pattern(query_number(&req, "n"))),
            "/file" | "/file-range" | "/file-short" => {
                let file = giong::fs::File::open(file_path()).await.unwrap();
                let len = file.len().unwrap();
                let body = match req.path() {
                    "/file" => Body::file(file).unwrap(),
                    "/file-range" => Body::file_range(file, 100, 1000),
                    // 100 bytes more than the file has.
                    _ => Body::file_range(file, 0, len + 100),
                };
                Response::new(200).body(body)
            }
            "/wait" => {
                let _guard = Guard(state.clone(), req.query().unwrap_or("").to_string());
                while !state.released.load(Ordering::SeqCst) {
                    sleep(Duration::from_millis(2)).await;
                }
                Response::text("released")
            }
            "/sleep" => {
                sleep(Duration::from_millis(query_number(&req, "ms") as u64)).await;
                Response::text("slept")
            }
            "/sleep-bytes" => {
                sleep(Duration::from_millis(query_number(&req, "ms") as u64)).await;
                Response::new(200).body(pattern(query_number(&req, "n")))
            }
            "/panic" => {
                sleep(Duration::from_millis(query_number(&req, "ms") as u64)).await;
                panic!("handler panic (expected by this test)");
            }
            "/pipe" | "/pipe-slow" => {
                let body = req.body_stream().expect("a streamed body");
                let mut total = 0;
                let mut first = true;
                loop {
                    match body.chunk().await {
                        Ok(Some(piece)) => total += piece.len(),
                        Ok(None) => break,
                        Err(e) => return Response::new(500).body(e.to_string()),
                    }
                    if first && req.path() == "/pipe-slow" {
                        first = false;
                        state.record("read one piece");
                        while !state.released.load(Ordering::SeqCst) {
                            sleep(Duration::from_millis(2)).await;
                        }
                    }
                }
                Response::text(format!("read {total}"))
            }
            "/stream" => {
                let (tx, body) = Body::channel();
                let pieces = query_number(&req, "n");
                drop(giong::spawn(async move {
                    for i in 0..pieces {
                        if tx.send(vec![b'a' + (i % 26) as u8; 1000]).await.is_err() {
                            return;
                        }
                    }
                }));
                Response::new(200).body(body)
            }
            "/big-headers" => {
                let mut resp = Response::new(200);
                for i in 0..40 {
                    resp = resp.header(&format!("x-big-{i}"), &"v".repeat(1000));
                }
                resp.body("big")
            }
            _ => Response::new(404),
        }
    }
}

struct TestServer {
    addr: SocketAddr,
    state: Arc<State>,
    handle: giong::Handle,
    thread: Option<JoinHandle<()>>,
}

impl TestServer {
    fn start(server: Server) -> Self {
        let app = App::default();
        let state = app.0.clone();
        let rt = giong::Builder::new()
            .workers(1)
            .shutdown_grace(Duration::from_secs(20))
            .build()
            .unwrap();
        let handle = rt.handle();
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            rt.run(move |_| {
                let (tx, server, app) = (tx.clone(), server.clone(), app.clone());
                async move {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    server.serve(listener, app).await;
                }
            })
            .unwrap();
        });
        Self {
            addr: rx.recv().unwrap(),
            state,
            handle,
            thread: Some(thread),
        }
    }

    /// A client that has exchanged SETTINGS with the server.
    fn client(&self) -> Client {
        self.client_with(&[])
    }

    fn client_with(&self, settings: &[(u16, u32)]) -> Client {
        let mut client = Client::connect(self.addr);
        client.handshake(settings);
        client
    }

    fn stop(&mut self) {
        self.handle.shutdown();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.state.released.store(true, Ordering::SeqCst);
        self.stop();
    }
}

// ------------------------------------------------------------------ client

#[derive(Debug)]
struct Frame {
    head: Head,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    done: bool,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap()
    }
}

/// How a stream or the connection ended instead of answering.
#[derive(Debug, PartialEq)]
enum Ended {
    Reset(u32),
    GoAway(u32, String),
    Closed,
}

struct Client {
    stream: TcpStream,
    buf: Vec<u8>,
    encoder: Encoder,
    decoder: Decoder,
    next_id: u32,
    /// Answers being received, by stream.
    replies: HashMap<u32, Reply>,
    /// Streams reset by the server, and its GOAWAY.
    resets: HashMap<u32, u32>,
    goaway: Option<(u32, u32, String)>,
    /// A header block being received: its stream, END_STREAM, and bytes.
    block: Option<(u32, bool, Vec<u8>)>,
    /// What the server may still send us.
    conn_window: i64,
    /// Grant window back as DATA arrives.
    auto_window: bool,
    /// What we may still send: the server's settings and windows.
    peer_initial: i64,
    send_conn: i64,
    send_windows: HashMap<u32, i64>,
    max_concurrent: u32,
}

impl Client {
    fn connect(addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_nodelay(true).unwrap();
        stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        Client {
            stream,
            buf: Vec::new(),
            encoder: Encoder::new(),
            decoder: Decoder::new(TABLE_SIZE),
            next_id: 1,
            replies: HashMap::new(),
            resets: HashMap::new(),
            goaway: None,
            block: None,
            conn_window: i64::from(frame::DEFAULT_WINDOW),
            auto_window: true,
            peer_initial: i64::from(frame::DEFAULT_WINDOW),
            send_conn: i64::from(frame::DEFAULT_WINDOW),
            send_windows: HashMap::new(),
            max_concurrent: u32::MAX,
        }
    }

    /// Sends the preface and our SETTINGS, and waits for the server's
    /// SETTINGS and its acknowledgement of ours.
    fn handshake(&mut self, settings: &[(u16, u32)]) {
        let mut out = PREFACE.to_vec();
        frame::write_settings(&mut out, settings);
        self.write(&out);
        let (mut theirs, mut acked) = (false, false);
        while !(theirs && acked) {
            let f = self.frame().expect("the server's SETTINGS");
            if f.head.kind == frame::SETTINGS {
                if f.head.has(frame::ACK) {
                    acked = true;
                } else {
                    theirs = true;
                }
            }
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        self.stream.write_all(bytes).unwrap();
    }

    /// Like `write`, but the server may have closed the connection.
    fn try_write(&mut self, bytes: &[u8]) -> bool {
        self.stream.write_all(bytes).is_ok()
    }

    fn send(&mut self, kind: u8, flags: u8, stream: u32, payload: &[u8]) {
        let mut out = Vec::new();
        frame::write_head(&mut out, payload.len(), kind, flags, stream);
        out.extend_from_slice(payload);
        self.write(&out);
    }

    fn encode(&mut self, fields: &[(&str, &str)]) -> Vec<u8> {
        let mut block = Vec::new();
        for &(name, value) in fields {
            self.encoder
                .field(name.as_bytes(), value.as_bytes(), Index::Yes, &mut block);
        }
        block
    }

    /// Opens a stream with a request head; returns its id.
    fn open(&mut self, method: &str, path: &str, extra: &[(&str, &str)], end_stream: bool) -> u32 {
        let id = self.next_id;
        self.next_id += 2;
        let mut fields = vec![
            (":method", method),
            (":scheme", "http"),
            (":path", path),
            (":authority", "example.com"),
        ];
        fields.extend_from_slice(extra);
        let block = self.encode(&fields);
        let mut out = Vec::new();
        frame::write_headers(&mut out, id, &block, end_stream, frame::DEFAULT_FRAME_SIZE);
        self.write(&out);
        self.send_windows.insert(id, self.peer_initial);
        id
    }

    fn get(&mut self, path: &str) -> u32 {
        self.open("GET", path, &[], true)
    }

    /// Sends a body in DATA frames as flow control allows, reading what
    /// comes meanwhile.
    fn send_body(&mut self, id: u32, mut body: &[u8], end: bool) {
        loop {
            let window = self.send_conn.min(self.send_windows[&id]).max(0) as usize;
            let n = body.len().min(window).min(frame::DEFAULT_FRAME_SIZE);
            if n == 0 && !body.is_empty() {
                // Wait for a WINDOW_UPDATE.
                let f = self.frame().expect("a WINDOW_UPDATE");
                assert!(!self.resets.contains_key(&id), "stream {id} reset: {f:?}");
                continue;
            }
            let last = n == body.len();
            let flags = if last && end { frame::END_STREAM } else { 0 };
            self.send(frame::DATA, flags, id, &body[..n]);
            self.send_conn -= n as i64;
            *self.send_windows.get_mut(&id).unwrap() -= n as i64;
            body = &body[n..];
            if last {
                return;
            }
        }
    }

    fn request(&mut self, method: &str, path: &str, extra: &[(&str, &str)], body: &[u8]) -> Reply {
        let id = self.open(method, path, extra, body.is_empty());
        if !body.is_empty() {
            self.send_body(id, body, true);
        }
        self.reply(id).unwrap()
    }

    /// Reads one frame and keeps track of what it means; `None` at the end
    /// of the connection.
    fn frame(&mut self) -> Option<Frame> {
        let f = self.read_frame()?;
        let (head, p) = (f.head, &f.payload[..]);
        match head.kind {
            frame::SETTINGS if !head.has(frame::ACK) => {
                for s in p.chunks_exact(6) {
                    let id = u16::from_be_bytes([s[0], s[1]]);
                    let value = frame::read_u32(&s[2..]);
                    match id {
                        frame::INITIAL_WINDOW_SIZE => {
                            let delta = i64::from(value) - self.peer_initial;
                            self.peer_initial = i64::from(value);
                            for w in self.send_windows.values_mut() {
                                *w += delta;
                            }
                        }
                        frame::MAX_CONCURRENT_STREAMS => self.max_concurrent = value,
                        _ => {}
                    }
                }
                let mut out = Vec::new();
                frame::write_settings_ack(&mut out);
                let _ = self.try_write(&out);
            }
            frame::WINDOW_UPDATE => {
                let inc = i64::from(frame::read_u32(p) & 0x7fff_ffff);
                if head.stream == 0 {
                    self.send_conn += inc;
                } else if let Some(w) = self.send_windows.get_mut(&head.stream) {
                    *w += inc;
                }
            }
            frame::HEADERS => {
                let fragment = frame::unpadded(&head, p).to_vec();
                let end = head.has(frame::END_STREAM);
                self.block = Some((head.stream, end, fragment));
                if head.has(frame::END_HEADERS) {
                    self.finish_block();
                }
            }
            frame::CONTINUATION => {
                let block = self.block.as_mut().expect("CONTINUATION after HEADERS");
                assert_eq!(block.0, head.stream);
                block.2.extend_from_slice(p);
                if head.has(frame::END_HEADERS) {
                    self.finish_block();
                }
            }
            frame::DATA => {
                let reply = self.replies.entry(head.stream).or_default();
                assert!(reply.status >= 200, "DATA before the response head");
                assert!(!reply.done, "DATA after END_STREAM");
                reply.body.extend_from_slice(frame::unpadded(&head, p));
                if head.has(frame::END_STREAM) {
                    reply.done = true;
                }
                self.conn_window -= p.len() as i64;
                assert!(
                    self.conn_window >= 0,
                    "the server overran the connection window"
                );
                if self.auto_window && !p.is_empty() {
                    let mut out = Vec::new();
                    frame::write_window_update(&mut out, 0, p.len() as u32);
                    if !head.has(frame::END_STREAM) {
                        frame::write_window_update(&mut out, head.stream, p.len() as u32);
                    }
                    self.conn_window += p.len() as i64;
                    let _ = self.try_write(&out);
                }
            }
            frame::RST_STREAM => {
                self.resets.insert(head.stream, frame::read_u32(p));
            }
            frame::GOAWAY => {
                let last = frame::read_u32(p) & 0x7fff_ffff;
                let code = frame::read_u32(&p[4..]);
                let debug = String::from_utf8_lossy(&p[8..]).into_owned();
                self.goaway = Some((last, code, debug));
            }
            frame::PING if !head.has(frame::ACK) => {
                let mut out = Vec::new();
                frame::write_ping(&mut out, true, p);
                let _ = self.try_write(&out);
            }
            _ => {}
        }
        Some(f)
    }

    fn finish_block(&mut self) {
        let (stream, end, block) = self.block.take().unwrap();
        let mut fields = Vec::new();
        self.decoder
            .decode(&block, |n, v| {
                fields.push((
                    String::from_utf8(n.to_vec()).unwrap(),
                    String::from_utf8_lossy(v).into_owned(),
                ))
            })
            .expect("the server's header block decodes");
        let reply = self.replies.entry(stream).or_default();
        let status: Option<u16> = fields
            .iter()
            .find(|(n, _)| n == ":status")
            .map(|(_, v)| v.parse().unwrap());
        match status {
            // A 1xx: interim.
            Some(status) if status < 200 => {
                reply.headers.push((":interim".into(), status.to_string()));
            }
            Some(status) => {
                reply.status = status;
                reply.headers.extend(fields);
            }
            None => panic!("a header block without :status"),
        }
        if end {
            reply.done = true;
        }
    }

    fn read_frame(&mut self) -> Option<Frame> {
        loop {
            if self.buf.len() >= frame::HEAD_LEN {
                let head = Head::parse(&self.buf);
                if self.buf.len() >= frame::HEAD_LEN + head.len {
                    let payload = self.buf[frame::HEAD_LEN..frame::HEAD_LEN + head.len].to_vec();
                    self.buf.drain(..frame::HEAD_LEN + head.len);
                    return Some(Frame { head, payload });
                }
            }
            let mut chunk = [0; 65536];
            match self.stream.read(&mut chunk) {
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::ConnectionReset => return None,
                Err(e) => panic!("reading: {e}"),
            }
        }
    }

    /// Reads until the answer on stream `id` is complete.
    fn reply(&mut self, id: u32) -> Result<Reply, Ended> {
        loop {
            if self.replies.get(&id).is_some_and(|r| r.done) {
                return Ok(self.replies.remove(&id).unwrap());
            }
            if let Some(&code) = self.resets.get(&id) {
                return Err(Ended::Reset(code));
            }
            if let Some((last, code, debug)) = &self.goaway {
                if id > *last || *code != 0 {
                    return Err(Ended::GoAway(*code, debug.clone()));
                }
            }
            if self.frame().is_none() {
                return Err(Ended::Closed);
            }
        }
    }

    /// Reads until the server sends GOAWAY (or closes).
    fn goaway(&mut self) -> Ended {
        loop {
            if let Some((_, code, debug)) = &self.goaway {
                return Ended::GoAway(*code, debug.clone());
            }
            if self.frame().is_none() {
                return Ended::Closed;
            }
        }
    }

    /// Frames that arrive within `wait`.
    fn frames_within(&mut self, wait: Duration) -> Vec<Frame> {
        self.stream.set_read_timeout(Some(wait)).unwrap();
        let mut frames = Vec::new();
        loop {
            if self.buf.len() >= frame::HEAD_LEN {
                let head = Head::parse(&self.buf);
                if self.buf.len() >= frame::HEAD_LEN + head.len {
                    frames.push(self.frame().unwrap());
                    continue;
                }
            }
            let mut chunk = [0; 65536];
            match self.stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
                Err(e) if e.kind() == ErrorKind::ConnectionReset => break,
                Err(e) => panic!("reading: {e}"),
            }
        }
        self.stream.set_read_timeout(Some(READ_TIMEOUT)).unwrap();
        frames
    }
}

fn reason(code: u32) -> &'static str {
    Reason::from_u32(code).map_or("unknown", Reason::name)
}

// ------------------------------------------------------------------- tests

#[test]
fn serves_requests_with_and_without_bodies() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();

    let reply = c.request("GET", "/", &[], b"");
    assert_eq!(reply.status, 200);
    assert_eq!(reply.text(), "hello");
    assert_eq!(reply.header("content-length"), Some("5"));
    assert!(reply.header("date").is_some());

    // A body over several DATA frames, and one larger than a window.
    let id = c.open("POST", "/echo", &[], false);
    c.send_body(id, b"one ", false);
    c.send_body(id, b"two ", false);
    c.send_body(id, b"three", true);
    assert_eq!(c.reply(id).unwrap().text(), "one two three");
    let big = pattern(700_000);
    assert_eq!(c.request("PUT", "/echo", &[], &big).body, big);

    // HEAD: the head of the GET, and no DATA.
    let reply = c.request("HEAD", "/", &[], b"");
    assert_eq!(
        (reply.status, reply.header("content-length")),
        (200, Some("5"))
    );
    assert!(reply.body.is_empty());

    // What the handler sees: an HTTP/2 request, `host` from :authority,
    // the cookies joined again.
    let reply = c.request(
        "GET",
        "/info?q=1",
        &[("cookie", "a=1"), ("x-a", "b"), ("cookie", "c=2")],
        b"",
    );
    assert_eq!(
        reply.text(),
        "GET /info Some(\"q=1\") HTTP/2 host=example.com cookie=a=1; c=2 x-a=b"
    );
    assert_eq!(c.request("GET", "/nothing", &[], b"").status, 404);
}

#[test]
fn trailers_continue_streamed_answers_and_large_header_blocks() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();

    // A body followed by trailers.
    let id = c.open("POST", "/echo", &[("te", "trailers")], false);
    c.send_body(id, b"abc", false);
    let block = c.encode(&[("x-checksum", "1")]);
    c.send(
        frame::HEADERS,
        frame::END_HEADERS | frame::END_STREAM,
        id,
        &block,
    );
    assert_eq!(c.reply(id).unwrap().text(), "abc");

    // `expect: 100-continue` gets a 100 before the body is sent.
    let id = c.open(
        "POST",
        "/echo",
        &[("expect", "100-continue"), ("content-length", "3")],
        false,
    );
    loop {
        c.frame().unwrap();
        if c.replies.get(&id).is_some_and(|r| !r.headers.is_empty()) {
            break;
        }
    }
    assert_eq!(c.replies[&id].header(":interim"), Some("100"));
    c.send_body(id, b"xyz", true);
    assert_eq!(c.reply(id).unwrap().text(), "xyz");

    // A body produced piece by piece.
    let reply = c.request("GET", "/stream?n=50", &[], b"");
    assert_eq!(reply.body.len(), 50_000);
    assert!(reply.header("content-length").is_none());

    // A head larger than a frame goes out in CONTINUATION frames.
    let reply = c.request("GET", "/big-headers", &[], b"");
    assert_eq!(reply.header("x-big-39"), Some(&*"v".repeat(1000)));
    assert_eq!(reply.text(), "big");
}

#[test]
fn responses_follow_the_clients_flow_control() {
    let server = TestServer::start(Server::new());
    let mut c = server.client_with(&[(frame::INITIAL_WINDOW_SIZE, 1000)]);
    c.auto_window = false;
    let id = c.get("/bytes?n=300000");
    let mut window = 1000i64;
    let mut received = 0;
    loop {
        let f = c.frame().unwrap();
        if f.head.kind == frame::DATA && f.head.stream == id {
            received += f.payload.len();
            window -= f.payload.len() as i64;
            assert!(window >= 0, "the server sent past the stream window");
            if f.head.has(frame::END_STREAM) {
                break;
            }
            if window == 0 {
                // Nothing more until the window opens.
                let more = c.frames_within(Duration::from_millis(50));
                assert!(more.iter().all(|f| f.head.kind != frame::DATA), "{more:?}");
                let mut out = Vec::new();
                frame::write_window_update(&mut out, id, 40_000);
                frame::write_window_update(&mut out, 0, 40_000);
                c.conn_window += 40_000;
                c.write(&out);
                window += 40_000;
            }
        }
    }
    assert_eq!(received, 300_000);
    assert_eq!(c.replies[&id].body, pattern(300_000));
}

#[test]
fn bodies_held_by_a_connection_stay_within_a_budget() {
    // Bodies of up to 64 KiB: the budget is then 1 MiB, half of it shared
    // by 100 streams at first.
    let server = TestServer::start(Server::new().max_body_size(64 * 1024));
    let mut c = server.client();
    assert_eq!(c.peer_initial, (1 << 20) / 200);
    let body = pattern(60_000);
    let mut ids = Vec::new();
    for _ in 0..100 {
        ids.push(c.open("POST", "/echo", &[("content-length", "60000")], false));
    }
    c.frames_within(Duration::from_millis(100));
    // No more than the budget, and the oldest stream's top-up. A hundred
    // streams at the protocol's 64 KiB would be 6.4 MiB, at full window
    // 25 MiB.
    let granted: i64 = ids.iter().map(|id| c.send_windows[id]).sum();
    assert!(
        granted <= (1 << 20) + 256 * 1024,
        "{granted} bytes of window granted"
    );
    // Sent in turn as the windows allow, every body gets through.
    for &id in &ids {
        c.send_body(id, &body, true);
    }
    for &id in &ids {
        assert_eq!(c.reply(id).unwrap().body, body);
    }
}

#[test]
fn streams_opened_before_our_settings_are_acknowledged_share_the_budget() {
    // A client that has not acknowledged our SETTINGS counts with the
    // protocol's 64 KiB windows: only as many streams may carry a body as
    // the 1 MiB budget holds.
    let server = TestServer::start(Server::new());
    let mut c = Client::connect(server.addr);
    let mut out = PREFACE.to_vec();
    frame::write_settings(&mut out, &[]);
    c.write(&out);
    let ids: Vec<u32> = (0..30)
        .map(|_| c.open("POST", "/echo", &[("content-length", "60000")], false))
        .collect();
    c.frames_within(Duration::from_millis(200));
    let refused: Vec<u32> = ids
        .iter()
        .copied()
        .filter(|id| c.resets.get(id) == Some(&(Reason::RefusedStream as u32)))
        .collect();
    assert_eq!(refused, ids[16..], "the streams past the budget");
    // The others get their bodies through; then the window is the budget's
    // share.
    let body = pattern(60_000);
    for &id in &ids[..16] {
        c.send_body(id, &body, true);
    }
    for &id in &ids[..16] {
        assert_eq!(c.reply(id).unwrap().body, body);
    }
    assert_eq!(c.peer_initial, (1 << 20) / 200);
    assert_eq!(c.request("POST", "/echo", &[], &body).body, body);
}

#[test]
fn responses_a_client_does_not_take_stay_within_a_budget() {
    let server = TestServer::start(Server::new());
    // The client takes nothing: each 1 MiB answer is held until it does.
    let mut c = server.client_with(&[(frame::INITIAL_WINDOW_SIZE, 0)]);
    c.auto_window = false;
    let ids: Vec<u32> = (0..20).map(|_| c.get("/bytes?n=1048576")).collect();
    let started = || server.state.started.load(Ordering::SeqCst);
    let answered = || server.state.answered.load(Ordering::SeqCst);
    // The first answer fills the budget: the other handlers wait.
    wait_until("the first handler", || started() == 1);
    c.frames_within(Duration::from_millis(200));
    assert_eq!(started(), 1);

    // Handlers that were already running when the budget filled up wait
    // too, instead of adding their answers.
    let mut d = server.client_with(&[(frame::INITIAL_WINDOW_SIZE, 0)]);
    d.auto_window = false;
    let waiting: Vec<u32> = (0..20)
        .map(|_| d.get("/sleep-bytes?ms=50&n=1048576"))
        .collect();
    // They all start (this connection holds nothing yet); the first to
    // wake from its sleep answers and fills the budget.
    wait_until("20 handlers and one answer", || {
        started() == 21 && answered() == 2
    });
    d.frames_within(Duration::from_millis(300));
    assert_eq!((started(), answered()), (21, 2));
    // Given up on: the handlers are dropped. (Between two of the resets,
    // a waiting handler may get to answer.)
    let mut out = Vec::new();
    for id in waiting {
        frame::write_rst(&mut out, id, Reason::Cancel);
    }
    d.write(&out);

    // As the client takes the answers, the others are made and sent in
    // turn, and every one arrives whole.
    c.auto_window = true;
    let mut out = Vec::new();
    frame::write_settings(&mut out, &[(frame::INITIAL_WINDOW_SIZE, 1 << 20)]);
    frame::write_window_update(&mut out, 0, 30 << 20);
    c.conn_window += 30 << 20;
    c.write(&out);
    for &id in &ids {
        let reply = c.reply(id).unwrap();
        assert_eq!(reply.body, pattern(1 << 20));
    }
}

#[test]
fn a_handler_panic_ends_its_stream_only() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();
    let slow = c.get("/sleep?ms=300");
    let panics = c.get("/panic?ms=50");
    let deciding = c.open("POST", "/panic-deciding", &[("content-length", "5")], false);
    c.send_body(deciding, b"hello", true);
    let reply = c.reply(panics).unwrap();
    assert_eq!(reply.status, 500);
    assert_eq!(reply.header("content-length"), Some("0"));
    assert_eq!(c.reply(deciding).unwrap().status, 500);
    // The other stream, and the connection, go on.
    assert_eq!(c.reply(slow).unwrap().text(), "slept");
    assert_eq!(c.request("GET", "/", &[], b"").text(), "hello");
    assert!(c.goaway.is_none(), "{:?}", c.goaway);
}

#[test]
fn a_streamed_body_gets_window_only_as_the_handler_reads() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();
    let id = c.open("POST", "/pipe-slow", &[], false);
    let body = pattern(2 << 20);
    // Send until the windows close.
    let mut sent = 0;
    loop {
        c.frames_within(Duration::from_millis(30));
        let window = c.send_conn.min(c.send_windows[&id]).max(0) as usize;
        if window == 0 {
            break;
        }
        let n = window.min(frame::DEFAULT_FRAME_SIZE);
        c.send(frame::DATA, 0, id, &body[sent..sent + n]);
        c.send_conn -= n as i64;
        *c.send_windows.get_mut(&id).unwrap() -= n as i64;
        sent += n;
    }
    assert!(server.state.wait_for("read one piece"));
    assert!(
        sent <= 300 * 1024,
        "{sent} bytes sent to a handler that reads none"
    );
    server.state.released.store(true, Ordering::SeqCst);
    c.send_body(id, &body[sent..], true);
    assert_eq!(c.reply(id).unwrap().text(), format!("read {}", body.len()));
}

#[test]
fn streams_beyond_the_limit_are_refused() {
    let server = TestServer::start(Server::new().max_concurrent_streams(2));
    let mut c = server.client();
    assert_eq!(c.max_concurrent, 2);
    let a = c.get("/wait?a");
    let b = c.get("/wait?b");
    let refused = c.get("/wait?c");
    assert_eq!(
        c.reply(refused).unwrap_err(),
        Ended::Reset(Reason::RefusedStream as u32)
    );
    server.state.released.store(true, Ordering::SeqCst);
    assert_eq!(c.reply(a).unwrap().text(), "released");
    assert_eq!(c.reply(b).unwrap().text(), "released");
}

#[test]
fn a_reset_drops_the_handler_at_once() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();
    let id = c.get("/wait?cancelled");
    let mut started = Instant::now();
    while server.state.started.load(Ordering::SeqCst) == 0 {
        assert!(started.elapsed() < Duration::from_secs(1));
        std::thread::sleep(Duration::from_millis(1));
    }
    started = Instant::now();
    c.send(
        frame::RST_STREAM,
        0,
        id,
        &(Reason::Cancel as u32).to_be_bytes(),
    );
    assert!(server.state.wait_for("dropped cancelled"));
    assert!(started.elapsed() < Duration::from_millis(500));
    // The connection goes on.
    assert_eq!(c.request("GET", "/", &[], b"").text(), "hello");
}

#[test]
fn rapid_resets_end_the_connection() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();
    let mut out = Vec::new();
    for _ in 0..1000 {
        let id = c.next_id;
        c.next_id += 2;
        let block = c.encode(&[
            (":method", "GET"),
            (":scheme", "http"),
            (":path", "/wait"),
            (":authority", "x"),
        ]);
        frame::write_headers(&mut out, id, &block, true, frame::DEFAULT_FRAME_SIZE);
        frame::write_rst(&mut out, id, Reason::Cancel);
    }
    c.try_write(&out);
    match c.goaway() {
        Ended::GoAway(code, debug) => {
            assert_eq!(reason(code), "ENHANCE_YOUR_CALM");
            assert!(debug.starts_with("GIONG-E1116"), "{debug}");
        }
        other => panic!("{other:?}"),
    }
    // Far fewer handlers ran than streams were opened.
    assert!(server.state.started.load(Ordering::SeqCst) <= 300);
}

#[test]
fn floods_of_frames_that_do_no_work_end_the_connection() {
    let server = TestServer::start(Server::new());
    type Write = fn(&mut Vec<u8>);
    let floods: [(&str, Write); 4] = [
        ("PING", |out| frame::write_ping(out, false, &[0; 8])),
        ("SETTINGS", |out| frame::write_settings(out, &[])),
        ("WINDOW_UPDATE", |out| frame::write_window_update(out, 0, 1)),
        ("PRIORITY", |out| {
            frame::write_head(out, 5, frame::PRIORITY, 0, 1001);
            out.extend_from_slice(&[0, 0, 0, 3, 16]);
        }),
    ];
    for (name, write) in floods {
        let mut c = server.client();
        let mut out = Vec::new();
        for _ in 0..3000 {
            write(&mut out);
        }
        c.try_write(&out);
        match c.goaway() {
            Ended::GoAway(code, debug) => {
                assert_eq!(reason(code), "ENHANCE_YOUR_CALM", "{name}");
                assert!(debug.starts_with("GIONG-E1117"), "{name}: {debug}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

#[test]
fn continuation_floods_end_the_connection() {
    let server = TestServer::start(Server::new());

    // Empty CONTINUATION frames, without end.
    let mut c = server.client();
    let block = c.encode(&[(":method", "GET"), (":scheme", "http"), (":path", "/")]);
    let mut out = Vec::new();
    frame::write_head(&mut out, block.len(), frame::HEADERS, 0, 1);
    out.extend_from_slice(&block);
    for _ in 0..3000 {
        frame::write_head(&mut out, 0, frame::CONTINUATION, 0, 1);
    }
    c.try_write(&out);
    match c.goaway() {
        Ended::GoAway(_, debug) => assert!(debug.starts_with("GIONG-E1117"), "{debug}"),
        other => panic!("{other:?}"),
    }

    // CONTINUATION frames that never end the block.
    let mut c = server.client();
    let mut out = Vec::new();
    frame::write_head(&mut out, block.len(), frame::HEADERS, 0, 1);
    out.extend_from_slice(&block);
    for _ in 0..100 {
        frame::write_head(&mut out, 4096, frame::CONTINUATION, 0, 1);
        out.extend_from_slice(&[0x40; 4096]);
    }
    c.try_write(&out);
    match c.goaway() {
        Ended::GoAway(code, debug) => {
            assert_eq!(reason(code), "ENHANCE_YOUR_CALM");
            assert!(debug.starts_with("GIONG-E1118"), "{debug}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_header_list_that_decodes_too_large_gets_431() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();
    let value = "a".repeat(4000);
    // Enters the dynamic table on the way.
    let reply = c.request("GET", "/", &[("x-bomb", &value)], b"");
    assert_eq!(reply.status, 200);
    // A few bytes on the wire for each copy, 4 KB once decoded.
    let copies: Vec<(&str, &str)> = (0..100).map(|_| ("x-bomb", &*value)).collect();
    let id = c.open("GET", "/", &copies, true);
    assert_eq!(c.reply(id).unwrap().status, 431);
    assert_eq!(c.request("GET", "/", &[], b"").status, 200);
}

#[test]
fn malformed_requests_are_reset_and_the_connection_goes_on() {
    let server = TestServer::start(Server::new());
    let mut c = server.client();
    for extra in [
        &[("X-Upper", "1")][..],
        &[("connection", "keep-alive")],
        &[("te", "gzip")],
        &[(":protocol", "x")],
    ] {
        let id = c.open("GET", "/", extra, true);
        assert_eq!(
            c.reply(id).unwrap_err(),
            Ended::Reset(Reason::Protocol as u32),
            "{extra:?}"
        );
    }
    // A body longer than its content-length.
    let id = c.open("POST", "/echo", &[("content-length", "2")], false);
    c.send_body(id, b"abc", true);
    assert_eq!(
        c.reply(id).unwrap_err(),
        Ended::Reset(Reason::Protocol as u32)
    );
    assert_eq!(c.request("GET", "/", &[], b"").status, 200);
}

#[test]
fn a_client_that_stops_reading_loses_its_stream() {
    let server = TestServer::start(Server::new().write_timeout(Duration::from_millis(200)));
    let mut c = server.client_with(&[(frame::INITIAL_WINDOW_SIZE, 0)]);
    let id = c.get("/bytes?n=1000");
    let start = Instant::now();
    assert_eq!(
        c.reply(id).unwrap_err(),
        Ended::Reset(Reason::Cancel as u32)
    );
    assert!(start.elapsed() >= Duration::from_millis(150));
}

#[test]
fn idle_connections_get_a_goaway() {
    let server = TestServer::start(Server::new().idle_timeout(Duration::from_millis(100)));
    let mut c = server.client();
    assert_eq!(c.request("GET", "/", &[], b"").status, 200);
    assert_eq!(c.goaway(), Ended::GoAway(0, String::new()));
    assert!(c.read_frame().is_none());
}

#[test]
fn shutdown_lets_open_streams_finish() {
    let mut server = TestServer::start(Server::new());
    let mut c = server.client();
    let id = c.get("/sleep?ms=200");
    while server.state.started.load(Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    server.handle.shutdown();
    assert_eq!(c.reply(id).unwrap().text(), "slept");
    let (last, code, _) = c.goaway.clone().expect("a GOAWAY");
    assert_eq!((last, code), (id, 0));
    assert!(c.read_frame().is_none());
    server.stop();
}

#[test]
fn http1_and_http2_share_a_port() {
    let server = TestServer::start(Server::new());
    let mut h1 = TcpStream::connect(server.addr).unwrap();
    h1.write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut answer = String::new();
    h1.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n") && answer.ends_with("hello"));
    let mut c = server.client();
    assert_eq!(c.request("GET", "/", &[], b"").text(), "hello");

    // Turned off, the preface is just a malformed HTTP/1.1 request; the
    // same with only prior knowledge turned off.
    for server in [
        Server::new().http2(false),
        Server::new().http2_prior_knowledge(false),
    ] {
        let server = TestServer::start(server);
        let mut c = Client::connect(server.addr);
        c.write(PREFACE);
        let mut answer = String::new();
        c.stream.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 505 "), "{answer}");
    }
}

#[test]
fn file_bodies_are_read_in_pieces_and_a_short_file_resets_its_stream() {
    let data = pattern(1_000_003);
    std::fs::write(file_path(), &data).unwrap();
    let server = TestServer::start(Server::new());
    let mut c = server.client();
    let reply = c.request("GET", "/file", &[], b"");
    assert_eq!(reply.header("content-length"), Some("1000003"));
    assert!(reply.body == data);
    let reply = c.request("GET", "/file-range", &[], b"");
    assert_eq!(reply.header("content-length"), Some("1000"));
    assert!(reply.body == data[100..1100]);
    let reply = c.request("HEAD", "/file", &[], b"");
    assert_eq!(reply.header("content-length"), Some("1000003"));
    assert!(reply.body.is_empty());

    // The file ends before the length its head announced: what it had,
    // then a reset; the connection carries on.
    let id = c.get("/file-short");
    assert_eq!(
        c.reply(id).unwrap_err(),
        Ended::Reset(Reason::Internal as u32)
    );
    assert!(c.replies.get(&id).is_some_and(|r| r.body == data));
    assert_eq!(c.request("GET", "/", &[], b"").text(), "hello");
    let _ = std::fs::remove_file(file_path());
}
