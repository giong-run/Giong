//! The cases: what one operation is, and how it is set up.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream, UdpSocket};
use giong_http::{Request, Response, Server};

use crate::Meter;

type Run = for<'a> fn(&'a Meter) -> Pin<Box<dyn Future<Output = ()> + 'a>>;

pub struct Case {
    pub name: &'static str,
    pub what: &'static str,
    pub run: Run,
}

macro_rules! case {
    ($name:literal, $what:literal, $f:path) => {
        Case {
            name: $name,
            what: $what,
            run: |m| Box::pin($f(m)),
        }
    };
}

pub const CASES: &[Case] = &[
    case!(
        "spawn",
        "spawn a task that returns at once and await it",
        spawn
    ),
    case!(
        "yield",
        "yield_now: one trip through the run queue",
        yield_now
    ),
    case!(
        "oneshot",
        "a oneshot channel: create, send, receive",
        oneshot
    ),
    case!(
        "channel",
        "a round trip between two tasks over two bounded channels",
        channel
    ),
    case!(
        "timeout",
        "a timeout around a future that is ready at once: arm and disarm a timer",
        timeout
    ),
    case!(
        "tcp-echo",
        "64 bytes to a TCP echo task and back over loopback",
        tcp_echo
    ),
    case!(
        "udp-echo",
        "a 64-byte datagram to a UDP echo task and back",
        udp_echo
    ),
    case!(
        "file-read",
        "read 4 KiB at an offset of an open file (page cache)",
        file_read
    ),
    case!(
        "http1",
        "giong-http server, HTTP/1.1 GET /json, raw keep-alive client",
        http1
    ),
    case!(
        "http1-client",
        "giong-http client to giong-http server, HTTP/1.1 GET /json",
        http1_client
    ),
    case!(
        "h2",
        "giong-http client to giong-http server, HTTP/2 (prior knowledge) GET /json",
        h2
    ),
    case!(
        "h2-streams",
        "giong-http client to giong-http server, ten HTTP/2 GET /json at once on one connection",
        h2_streams
    ),
    case!(
        "https",
        "giong-http client to giong-http server over TLS (rustls), HTTP/1.1 GET /json",
        https
    ),
    case!(
        "web",
        "giong-web router (3 routes, a path parameter) on giong-http, raw client",
        web
    ),
    case!(
        "redis-get",
        "giong-redis GET against an in-process fake server",
        crate::db::redis_get
    ),
    case!(
        "postgres-query",
        "giong-postgres query with one parameter and one row, fake server",
        crate::db::postgres_query
    ),
    case!(
        "mysql-query",
        "giong-mysql query with one parameter and one row, fake server",
        crate::db::mysql_query
    ),
];

type Idle = for<'a> fn(&'static str, usize) -> Pin<Box<dyn Future<Output = ()>>>;

pub struct IdleCase {
    pub name: &'static str,
    pub what: &'static str,
    pub run: Idle,
}

pub const IDLE: &[IdleCase] = &[
    IdleCase {
        name: "idle-http1",
        what: "heap and RSS per idle HTTP/1.1 connection of giong-http's server",
        run: |name, n| Box::pin(idle(name, n, Proto::Http1)),
    },
    IdleCase {
        name: "idle-h2",
        what: "heap and RSS per idle HTTP/2 connection, after one request",
        run: |name, n| Box::pin(idle(name, n, Proto::Http2)),
    },
    IdleCase {
        name: "idle-https",
        what: "heap and RSS per idle HTTPS (HTTP/1.1 over rustls) connection, after one request",
        run: |name, n| Box::pin(idle(name, n, Proto::Https)),
    },
];

#[derive(Clone, Copy, PartialEq)]
enum Proto {
    Http1,
    Http2,
    Https,
}

async fn spawn(m: &Meter) {
    for i in m.iter() {
        let v = giong::spawn(async move { i })
            .await
            .expect("the task returned");
        std::hint::black_box(v);
    }
}

async fn yield_now(m: &Meter) {
    for _ in m.iter() {
        giong::yield_now().await;
    }
}

async fn oneshot(m: &Meter) {
    for i in m.iter() {
        let (tx, rx) = giong::sync::oneshot::channel();
        tx.send(i).expect("the receiver is alive");
        std::hint::black_box(rx.await.expect("sent"));
    }
}

async fn channel(m: &Meter) {
    let (to_echo, mut echo_rx) = giong::sync::mpsc::bounded::<u64>(16);
    let (back, mut rx) = giong::sync::mpsc::bounded::<u64>(16);
    let echo = giong::spawn(async move {
        while let Some(v) = echo_rx.recv().await {
            if back.send(v).await.is_err() {
                return;
            }
        }
    });
    for i in m.iter() {
        to_echo.send(i).await.expect("the echo task is alive");
        std::hint::black_box(rx.recv().await.expect("echoed"));
    }
    drop(to_echo);
    let _ = echo.await;
}

async fn timeout(m: &Meter) {
    for i in m.iter() {
        let v = giong::time::timeout(Duration::from_secs(60), async move { i }).await;
        std::hint::black_box(v.expect("ready at once"));
    }
}

/// A listener on a free loopback port, and its address.
fn listen() -> (TcpListener, std::net::SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let addr = listener.local_addr().expect("its address");
    (listener, addr)
}

async fn tcp_echo(m: &Meter) {
    let (listener, addr) = listen();
    let server = giong::spawn(async move {
        let stream = listener.accept().await.expect("accept");
        loop {
            let Ok(data) = stream.recv().await else {
                return;
            };
            if data.is_empty() {
                return;
            }
            let reply = data.to_vec();
            drop(data);
            if stream.write_all(reply).await.0.is_err() {
                return;
            }
        }
    });
    let stream = TcpStream::connect(addr).await.expect("connect");
    stream.set_nodelay(true).expect("nodelay");
    static PING: [u8; 64] = [7; 64];
    for _ in m.iter() {
        stream.write_all(&PING[..]).await.0.expect("write");
        let mut got = 0;
        while got < PING.len() {
            let data = stream.recv().await.expect("recv");
            assert!(!data.is_empty(), "the echo task closed");
            got += data.len();
        }
    }
    drop(stream);
    let _ = server.await;
}

async fn udp_echo(m: &Meter) {
    let server = UdpSocket::bind("127.0.0.1:0").expect("bind");
    let addr = server.local_addr().expect("its address");
    let echo = giong::spawn(async move {
        loop {
            let Ok((data, from)) = server.recv_from().await else {
                return;
            };
            if data.is_empty() {
                return;
            }
            let reply = data.to_vec();
            drop(data);
            if server.send_to(reply, from).await.0.is_err() {
                return;
            }
        }
    });
    let client = UdpSocket::bind("127.0.0.1:0").expect("bind");
    client.connect(addr).expect("connect");
    static PING: [u8; 64] = [7; 64];
    for _ in m.iter() {
        client.send(&PING[..]).await.0.expect("send");
        let data = client.recv().await.expect("recv");
        assert_eq!(data.len(), PING.len());
    }
    client
        .send(&[][..])
        .await
        .0
        .expect("send the stop datagram");
    let _ = echo.await;
}

async fn file_read(m: &Meter) {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("giong-instr-{}", std::process::id()));
    std::fs::write(&path, vec![b'x'; 1 << 20]).expect("write the file");
    let file = giong::fs::File::open(&path).await.expect("open");
    let mut buf = vec![0u8; 4096];
    for i in m.iter() {
        let offset = (i % 256) * 4096;
        let (n, b) = file.read_at(buf, offset).await;
        assert_eq!(n.expect("read"), 4096);
        buf = b;
    }
    drop(file);
    let _ = std::fs::remove_file(path);
}

async fn json(_req: Request) -> Response {
    Response::json(r#"{"message":"Hello, World!"}"#)
}

/// Sends `request` and reads one response of `len` bytes (learnt from the
/// first response) over a raw connection.
struct RawClient {
    stream: TcpStream,
    request: &'static [u8],
    len: usize,
}

impl RawClient {
    async fn connect(addr: std::net::SocketAddr, request: &'static [u8]) -> Self {
        let stream = TcpStream::connect(addr).await.expect("connect");
        stream.set_nodelay(true).expect("nodelay");
        let mut c = RawClient {
            stream,
            request,
            len: 0,
        };
        // The first response: find the end of its head and its length.
        c.stream.write_all(request).await.0.expect("write");
        let mut got = Vec::new();
        loop {
            let data = c.stream.recv().await.expect("recv");
            assert!(!data.is_empty(), "the server closed");
            got.extend_from_slice(&data);
            let Some(end) = find(&got, b"\r\n\r\n") else {
                continue;
            };
            let head = std::str::from_utf8(&got[..end]).expect("an ASCII head");
            assert!(
                head.starts_with("HTTP/1.1 200"),
                "unexpected response: {head}"
            );
            let body: usize = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .expect("a Content-Length");
            if got.len() >= end + 4 + body {
                assert_eq!(got.len(), end + 4 + body, "one response at a time");
                c.len = got.len();
                return c;
            }
        }
    }

    async fn round_trip(&self) {
        self.stream.write_all(self.request).await.0.expect("write");
        let mut got = 0;
        while got < self.len {
            let data = self.stream.recv().await.expect("recv");
            assert!(!data.is_empty(), "the server closed");
            got += data.len();
        }
        assert_eq!(got, self.len, "responses of one length");
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn http1(m: &Meter) {
    let (listener, addr) = listen();
    let _server = giong::spawn(async move { Server::new().serve(listener, json).await });
    let client = RawClient::connect(addr, b"GET /json HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    for _ in m.iter() {
        client.round_trip().await;
    }
}

async fn client_loop(m: &Meter, client: giong_http::client::Client, url: &str) {
    for _ in m.iter() {
        let resp = client.get(url).send().await.expect("a response");
        assert_eq!(resp.status(), 200);
        std::hint::black_box(resp.body().len());
    }
}

async fn http1_client(m: &Meter) {
    let (listener, addr) = listen();
    let _server = giong::spawn(async move { Server::new().serve(listener, json).await });
    let client = giong_http::client::Client::builder().http2(false).build();
    client_loop(m, client, &format!("http://{addr}/json")).await;
}

async fn h2(m: &Meter) {
    let (listener, addr) = listen();
    let _server = giong::spawn(async move { Server::new().serve(listener, json).await });
    let client = giong_http::client::Client::builder()
        .http2_prior_knowledge(true)
        .build();
    let url = format!("http://{addr}/json");
    let first = client.get(&url).send().await.expect("a response");
    assert_eq!(
        first.version(),
        giong_http::Version::Http2,
        "HTTP/2 was used"
    );
    client_loop(m, client, &url).await;
}

/// Requests in flight together in `h2-streams`.
const STREAMS: usize = 10;

/// One operation is `STREAMS` requests sent together over one HTTP/2
/// connection, as from a client with many streams in flight. Requests that
/// arrive in one read queue up for their handlers, which `h2` (one request
/// at a time) never does: 0.8.0's fix for idle memory gave the queue back
/// each time it emptied, which cost 2% more instructions per request with
/// ten streams and 4.5% with a hundred, unseen here until this case.
async fn h2_streams(m: &Meter) {
    let (listener, addr) = listen();
    let _server = giong::spawn(async move { Server::new().serve(listener, json).await });
    let client = giong_http::client::Client::builder()
        .http2_prior_knowledge(true)
        .build();
    let url = std::rc::Rc::<str>::from(format!("http://{addr}/json"));
    let first = client.get(&url).send().await.expect("a response");
    assert_eq!(
        first.version(),
        giong_http::Version::Http2,
        "HTTP/2 was used"
    );
    let mut tasks = Vec::with_capacity(STREAMS);
    for _ in m.iter() {
        for _ in 0..STREAMS {
            let (client, url) = (client.clone(), url.clone());
            tasks.push(giong::spawn(async move {
                let resp = client.get(&url).send().await.expect("a response");
                assert_eq!(resp.status(), 200);
                resp.body().len()
            }));
        }
        for task in tasks.drain(..) {
            std::hint::black_box(task.await.expect("the request task"));
        }
    }
}

async fn https(m: &Meter) {
    let (listener, addr) = listen();
    // A fresh P-256 certificate: the handshake is not counted, and the
    // records' cost does not depend on the key.
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("a key");
    let cert = key.cert.pem();
    let acceptor =
        giong_tls::TlsAcceptor::from_pem(cert.as_bytes(), key.key_pair.serialize_pem().as_bytes())
            .expect("the test certificate");
    let _server =
        giong::spawn(async move { Server::new().serve_tls(listener, acceptor, json).await });
    let connector = giong_tls::TlsConnector::with_root_pem(cert.as_bytes()).expect("the test root");
    let client = giong_http::client::Client::builder()
        .tls(connector)
        .http2(false)
        .build();
    client_loop(
        m,
        client,
        &format!("https://localhost:{}/json", addr.port()),
    )
    .await;
}

async fn web(m: &Meter) {
    use giong_web::{Error, Req, Router};
    async fn user(req: Req<()>) -> Result<giong_web::Response, Error> {
        let id: u64 = req.param("id")?;
        Ok(giong_web::Response::json(format!(
            r#"{{"id":{id},"name":"Ada"}}"#
        )))
    }
    async fn health(_req: Req<()>) -> Result<giong_web::Response, Error> {
        Ok(giong_web::Response::text("ok"))
    }
    let (listener, addr) = listen();
    let _server = giong::spawn(async move {
        let app = Router::new()
            .get("/health", health)
            .get("/users/{id}", user)
            .post("/users", health)
            .build(());
        giong_web::Server::new().serve(listener, app).await
    });
    let client =
        RawClient::connect(addr, b"GET /users/42 HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    for _ in m.iter() {
        client.round_trip().await;
    }
}

/// Opens `n` idle connections to a giong-http server in this process from
/// another thread (blocking std sockets, which own no heap memory) and
/// prints the server's heap and RSS growth per connection.
///
/// For HTTPS the other thread does each handshake and one keep-alive
/// request with rustls, then drops the client's TLS state and keeps only
/// the socket (the server cannot tell, nothing is sent), so the heap that
/// stays is the server's. The allocation count covers both ends.
async fn idle(name: &'static str, n: usize, proto: Proto) {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    let (listener, addr) = listen();
    let mut tls_client = None;
    if proto == Proto::Https {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("a key");
        let cert = key.cert.pem();
        let acceptor = giong_tls::TlsAcceptor::from_pem(
            cert.as_bytes(),
            key.key_pair.serialize_pem().as_bytes(),
        )
        .expect("the test certificate");
        let _server =
            giong::spawn(async move { Server::new().serve_tls(listener, acceptor, json).await });
        // One request first, so whatever the server sets up once is not
        // counted.
        let connector =
            giong_tls::TlsConnector::with_root_pem(cert.as_bytes()).expect("the test root");
        let client = giong_http::client::Client::builder()
            .tls(connector)
            .http2(false)
            .build();
        let url = format!("https://localhost:{}/json", addr.port());
        let resp = client.get(&url).send().await.expect("a response");
        assert_eq!(resp.status(), 200);
        drop((resp, client));
        let mut roots = rustls::RootCertStore::empty();
        roots.add(key.cert.der().clone()).expect("the test root");
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        // No session cache: it would keep tickets on the client's heap.
        config.resumption = rustls::client::Resumption::disabled();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        tls_client = Some(Arc::new(config));
    } else {
        let _server = giong::spawn(async move { Server::new().serve(listener, json).await });
        let warm = RawClient::connect(addr, b"GET /json HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
        drop(warm);
    }
    let go = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let opener = {
        let (go, ready, done, stop) = (go.clone(), ready.clone(), done.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut conns = Vec::with_capacity(n);
            while !go.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            for _ in 0..n {
                let mut s = std::net::TcpStream::connect(addr).expect("connect");
                match proto {
                    Proto::Http1 => {}
                    Proto::Http2 => h2_request(&mut s),
                    Proto::Https => {
                        let config = tls_client.clone().expect("the client config");
                        tls_request(config, &mut s);
                    }
                }
                conns.push(s);
                ready.fetch_add(1, Ordering::Release);
            }
            done.store(true, Ordering::Release);
            while !stop.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(conns);
        })
    };
    // Let the server settle after the warm-up connection closed.
    giong::time::sleep(Duration::from_millis(200)).await;
    let tasks0 = giong::observe::worker_stats().tasks;
    let (heap0, rss0, allocs0) = (crate::live_bytes(), crate::rss(), crate::allocations());
    go.store(true, Ordering::Release);
    // Every connection has its task once the server has accepted it.
    while giong::observe::worker_stats().tasks < tasks0 + n || !done.load(Ordering::Acquire) {
        giong::time::sleep(Duration::from_millis(5)).await;
    }
    giong::time::sleep(Duration::from_millis(200)).await;
    let (heap1, rss1, allocs1) = (crate::live_bytes(), crate::rss(), crate::allocations());
    let tasks1 = giong::observe::worker_stats().tasks;
    println!(
        "case={name} connections={n} tasks={} heap_bytes_per_conn={:.1} rss_bytes_per_conn={:.1} allocations_per_conn={:.2}",
        tasks1 - tasks0,
        (heap1 as f64 - heap0 as f64) / n as f64,
        (rss1 as f64 - rss0 as f64) / n as f64,
        (allocs1 - allocs0) as f64 / n as f64,
    );
    stop.store(true, Ordering::Release);
    opener.join().expect("the opener thread");
}

/// The HTTP/2 preface, SETTINGS and one `GET /` over `sock` (prior
/// knowledge), read to the end of the response, as `bench/h2.sh`'s `h2-idle`
/// does: a connection that has made a request holds what a request leaves
/// behind (an HTTP/2 connection kept the memory for its streams for a second
/// after each request in 0.8.0, which only shows when the connection has
/// had one). Waiting for the answer before the next connection also keeps
/// the server from falling behind: with thousands of prefaces queued, the
/// receive buffer pool runs low and the driver copies data to the heap, by
/// timing.
fn h2_request(sock: &mut std::net::TcpStream) {
    use std::io::{Read, Write};
    // HEADERS on stream 1, END_STREAM and END_HEADERS: :method GET,
    // :scheme http, :path / (static table entries 2, 6 and 4) and
    // :authority localhost (literal, not indexed, name from entry 1).
    const HEADERS: &[u8] = b"\0\0\x0e\x01\x05\0\0\0\x01\x82\x86\x84\x01\x09localhost";
    let mut out = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0".to_vec();
    out.extend_from_slice(HEADERS);
    sock.write_all(&out)
        .expect("write the preface and the request");
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        // Every complete frame so far.
        let mut at = 0;
        while got.len() >= at + 9 {
            let len = usize::from(got[at]) << 16
                | usize::from(got[at + 1]) << 8
                | usize::from(got[at + 2]);
            if got.len() < at + 9 + len {
                break;
            }
            let (kind, flags) = (got[at + 3], got[at + 4]);
            let stream = u32::from_be_bytes([got[at + 5], got[at + 6], got[at + 7], got[at + 8]]);
            if kind == 4 && flags & 1 == 0 {
                sock.write_all(b"\0\0\0\x04\x01\0\0\0\0")
                    .expect("acknowledge SETTINGS");
            }
            if (kind == 0 || kind == 1) && stream == 1 && flags & 1 == 1 {
                return;
            }
            assert!(kind != 7, "the server sent GOAWAY");
            at += 9 + len;
        }
        got.drain(..at);
        let k = sock.read(&mut buf).expect("read the response");
        assert!(k > 0, "the server closed");
        got.extend_from_slice(&buf[..k]);
    }
}

/// A TLS handshake and one keep-alive `GET /json` over `sock`, read to the
/// end of the response; the client's TLS state is then dropped.
fn tls_request(config: std::sync::Arc<rustls::ClientConfig>, sock: &mut std::net::TcpStream) {
    use std::io::{Read, Write};
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let mut conn = rustls::ClientConnection::new(config, name).expect("a TLS client");
    let mut tls = rustls::Stream::new(&mut conn, sock);
    tls.write_all(b"GET /json HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .expect("write the request");
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let k = tls.read(&mut buf).expect("read the response");
        assert!(k > 0, "the server closed");
        got.extend_from_slice(&buf[..k]);
        let Some(end) = find(&got, b"\r\n\r\n") else {
            continue;
        };
        let head = std::str::from_utf8(&got[..end]).expect("an ASCII head");
        assert!(
            head.starts_with("HTTP/1.1 200"),
            "unexpected response: {head}"
        );
        let body: usize = head
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .expect("a Content-Length");
        if got.len() >= end + 4 + body {
            return;
        }
    }
}
