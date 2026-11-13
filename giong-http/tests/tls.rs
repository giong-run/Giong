//! HTTPS end to end: `Server::serve_tls` on a real socket, driven by a
//! blocking rustls client.

use std::future::Future;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong::time::sleep;
use giong_http::{Request, Response, Server};
use giong_tls::TlsAcceptor;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

// ------------------------------------------------------------------ harness

struct TestServer {
    addr: SocketAddr,
    handle: giong::Handle,
    thread: Option<JoinHandle<()>>,
    /// Trusts only the server's self-signed certificate.
    roots: RootCertStore,
    client_config: Arc<ClientConfig>,
}

fn client_config(
    roots: RootCertStore,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Arc<ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

impl TestServer {
    fn start<H, Fut>(server: Server, handler: H) -> Self
    where
        H: Fn(Request) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Response> + 'static,
    {
        // Generous: under a sanitizer a handshake takes a while.
        Self::start_with(server, handler, Duration::from_secs(30))
    }

    fn start_with<H, Fut>(server: Server, handler: H, handshake_timeout: Duration) -> Self
    where
        H: Fn(Request) -> Fut + Clone + Send + Sync + 'static,
        Fut: Future<Output = Response> + 'static,
    {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let acceptor = TlsAcceptor::from_pem(
            key.cert.pem().as_bytes(),
            key.key_pair.serialize_pem().as_bytes(),
        )
        .unwrap()
        .handshake_timeout(handshake_timeout);
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from(key.cert.der().to_vec()))
            .unwrap();

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
                let acceptor = acceptor.clone();
                async move {
                    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    server.serve_tls(listener, acceptor, handler).await;
                }
            })
            .unwrap();
        });
        Self {
            addr: rx.recv().unwrap(),
            handle,
            thread: Some(thread),
            client_config: client_config(roots.clone(), rustls::ALL_VERSIONS),
            roots,
        }
    }

    fn tcp(&self) -> TcpStream {
        let tcp = TcpStream::connect(self.addr).unwrap();
        tcp.set_nodelay(true).unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        tcp
    }

    fn client_with(&self, config: &Arc<ClientConfig>) -> Client {
        let name = ServerName::try_from("localhost").unwrap();
        let conn = ClientConnection::new(config.clone(), name).unwrap();
        Client {
            stream: StreamOwned::new(conn, self.tcp()),
            buf: Vec::new(),
        }
    }

    fn client(&self) -> Client {
        self.client_with(&self.client_config)
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
    stream: StreamOwned<ClientConnection, TcpStream>,
    buf: Vec<u8>,
}

impl Client {
    fn send(&mut self, bytes: impl AsRef<[u8]>) {
        self.stream.write_all(bytes.as_ref()).unwrap();
        self.stream.flush().unwrap();
    }

    /// Reads more input; `false` once the server has closed the TLS session
    /// with `close_notify`. A connection that just disappears is an error.
    fn fill(&mut self) -> bool {
        self.try_fill().expect("the connection was cut off")
    }

    /// Like `fill`, but `None` when the connection is cut off.
    fn try_fill(&mut self) -> Option<bool> {
        let mut chunk = [0u8; 16 * 1024];
        match self.stream.read(&mut chunk) {
            Ok(0) => Some(false),
            Ok(n) => {
                self.buf.extend_from_slice(&chunk[..n]);
                Some(true)
            }
            Err(_) => None,
        }
    }

    fn reply(&mut self) -> Reply {
        self.try_reply()
            .expect("connection closed before a full response")
    }

    /// Reads one response; `None` if the connection ends first.
    fn try_reply(&mut self) -> Option<Reply> {
        let end = loop {
            if let Some(i) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            if !self.try_fill()? {
                return None;
            }
        };
        let head = String::from_utf8(self.buf[..end].to_vec()).unwrap();
        let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
        let mut reply = Reply {
            status,
            head,
            body: Vec::new(),
        };
        let len: usize = reply
            .header("content-length")
            .map_or(0, |v| v.parse().unwrap());
        while self.buf.len() < end + len {
            if !self.try_fill()? {
                return None;
            }
        }
        reply.body = self.buf[end..end + len].to_vec();
        self.buf.drain(..end + len);
        Some(reply)
    }

    /// Asserts that the server ends the TLS session cleanly, sending nothing
    /// more.
    fn expect_closed(&mut self) {
        while self.fill() {}
        assert!(
            self.buf.is_empty(),
            "unexpected data before close: {:?}",
            String::from_utf8_lossy(&self.buf)
        );
    }
}

async fn echo(req: Request) -> Response {
    let text = format!(
        "{} {} body={}",
        req.method(),
        req.path(),
        String::from_utf8_lossy(req.body()),
    );
    Response::text(text)
}

// -------------------------------------------------------------------- tests

#[test]
fn answers_requests_on_a_kept_alive_tls_connection() {
    let server = TestServer::start(Server::new(), echo);
    let mut c = server.client();
    c.send("GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let r = c.reply();
    assert_eq!((r.status, r.text()), (200, "GET /hello body="));
    assert_eq!(r.header("connection"), None);

    c.send("POST /post HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\n\r\nhello");
    assert_eq!(c.reply().text(), "POST /post body=hello");

    c.send(
        "POST /chunked HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n\
         3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n",
    );
    assert_eq!(c.reply().text(), "POST /chunked body=abcde");

    // `Connection: close` ends the TLS session properly, not just the socket.
    c.send("GET /bye HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.header("connection"), Some("close"));
    c.expect_closed();
}

#[test]
fn works_with_tls_1_2_and_tls_1_3_clients() {
    let server = TestServer::start(Server::new(), echo);
    for version in [&rustls::version::TLS12, &rustls::version::TLS13] {
        let config = client_config(server.roots.clone(), &[version]);
        let mut c = server.client_with(&config);
        c.send("GET /v HTTP/1.1\r\nHost: localhost\r\n\r\n");
        assert_eq!(c.reply().text(), "GET /v body=");
        assert_eq!(
            c.stream.conn.protocol_version(),
            Some(version.version),
            "the client did not get the version it asked for"
        );
    }
}

#[test]
fn pipelined_requests_are_answered_in_order_over_tls() {
    let server = TestServer::start(Server::new(), echo);
    let mut c = server.client();
    let mut batch = String::new();
    for i in 0..200 {
        batch.push_str(&format!("GET /p/{i} HTTP/1.1\r\nHost: localhost\r\n\r\n"));
    }
    c.send(batch);
    for i in 0..200 {
        assert_eq!(c.reply().text(), format!("GET /p/{i} body="));
    }
}

#[test]
fn large_bodies_survive_in_both_directions_over_tls() {
    async fn reverse(req: Request) -> Response {
        let mut body = req.body().to_vec();
        body.reverse();
        Response::new(200).body(body)
    }
    let server = TestServer::start(Server::new().max_body_size(4 << 20), reverse);
    let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    let mut c = server.client();
    c.send(format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
        body.len()
    ));
    c.send(&body);
    let r = c.reply();
    let mut expected = body;
    expected.reverse();
    assert!(r.body == expected);
}

#[test]
fn file_bodies_are_encrypted_and_sent_over_tls() {
    let data: Vec<u8> = (0..1_500_007u32).map(|i| (i * 13 % 251) as u8).collect();
    let path = std::env::temp_dir().join(format!("giong-http-tls-{}-file", std::process::id()));
    std::fs::write(&path, &data).unwrap();
    let served = path.clone();
    let handler = move |req: Request| {
        let path = served.clone();
        async move {
            let file = giong::fs::File::open(&path).await.unwrap();
            let body = match req.path() {
                "/range" => giong_http::Body::file_range(file, 1_000, 70_000),
                _ => giong_http::Body::file(file).unwrap(),
            };
            Response::new(200).body(body)
        }
    };
    let server = TestServer::start(Server::new(), handler);
    let mut c = server.client();
    c.send("GET /file HTTP/1.1\r\nHost: localhost\r\n\r\nGET /range HTTP/1.1\r\nHost: localhost\r\n\r\n");
    let r = c.reply();
    assert_eq!(r.header("content-length"), Some("1500007"));
    assert!(r.body == data);
    assert!(c.reply().body == data[1_000..71_000]);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn errors_are_answered_and_the_session_is_closed() {
    let server = TestServer::start(Server::new().max_body_size(10), echo);
    let mut c = server.client();
    c.send("GET / HTTP/1.1\r\n\r\n"); // no Host
    assert_eq!(c.reply().status, 400);
    c.expect_closed();

    let mut c = server.client();
    c.send("POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 11\r\n\r\n");
    assert_eq!(c.reply().status, 413);
    c.expect_closed();
}

#[test]
fn idle_tls_connections_are_closed() {
    let server = TestServer::start(Server::new().idle_timeout(Duration::from_millis(200)), echo);
    let mut c = server.client();
    c.send("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(c.reply().status, 200);
    let start = Instant::now();
    c.expect_closed();
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn clients_that_do_not_speak_tls_are_dropped() {
    let server = TestServer::start_with(Server::new(), echo, Duration::from_millis(500));

    // Plain HTTP on the HTTPS port gets no HTTP answer.
    let mut plain = server.tcp();
    plain
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    let mut reply = Vec::new();
    let _ = plain.read_to_end(&mut reply);
    assert!(!reply.starts_with(b"HTTP/"));

    // A connection that never starts a handshake is closed after the
    // handshake timeout (500 ms here), not kept for the idle timeout.
    let mut silent = server.tcp();
    let start = Instant::now();
    let mut buf = [0u8; 16];
    match silent.read(&mut buf) {
        Ok(0) => {}
        Err(e) if e.kind() == ErrorKind::ConnectionReset => {}
        other => panic!("expected the connection to be closed: {other:?}"),
    }
    assert!(start.elapsed() >= Duration::from_millis(400));
    assert!(start.elapsed() < Duration::from_secs(5));

    // A client that does not trust the certificate gives up by itself.
    let distrustful = client_config(RootCertStore::empty(), rustls::ALL_VERSIONS);
    let mut c = server.client_with(&distrustful);
    assert!(c.stream.write_all(b"GET / HTTP/1.1\r\n\r\n").is_err());

    // None of that disturbed the server. (The handshake may take longer than
    // the short timeout on a slow machine, so try a few times.)
    let answered = (0..20).any(|_| {
        let mut c = server.client();
        let request = b"GET /ok HTTP/1.1\r\nHost: localhost\r\n\r\n";
        c.stream.write_all(request).is_ok()
            && c.stream.flush().is_ok()
            && c.try_reply().is_some_and(|r| r.text() == "GET /ok body=")
    });
    assert!(answered);
}

#[test]
fn shutdown_finishes_requests_in_progress_over_tls() {
    async fn handler(req: Request) -> Response {
        if req.path() == "/slow" {
            sleep(Duration::from_millis(400)).await;
        }
        Response::text("done")
    }
    let mut server = TestServer::start(Server::new(), handler);
    let mut idle = server.client();
    idle.send("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(idle.reply().text(), "done");

    let mut busy = server.client();
    busy.send("GET /slow HTTP/1.1\r\nHost: localhost\r\n\r\n");
    std::thread::sleep(Duration::from_millis(100));

    let stopped_after = server.stop();

    let r = busy.reply();
    assert_eq!((r.status, r.text()), (200, "done"));
    assert_eq!(r.header("connection"), Some("close"));
    busy.expect_closed();
    idle.expect_closed();
    assert!(
        stopped_after >= Duration::from_millis(250),
        "{stopped_after:?}"
    );
    assert!(stopped_after < Duration::from_secs(10), "{stopped_after:?}");
}

#[test]
fn many_tls_connections_at_once() {
    let server = Arc::new(TestServer::start(Server::new(), echo));
    let threads: Vec<_> = (0..16)
        .map(|t| {
            let server = server.clone();
            std::thread::spawn(move || {
                for i in 0..20 {
                    let mut c = server.client();
                    for j in 0..5 {
                        c.send(format!(
                            "GET /{t}/{i}/{j} HTTP/1.1\r\nHost: localhost\r\n\r\n"
                        ));
                        assert_eq!(c.reply().text(), format!("GET /{t}/{i}/{j} body="));
                    }
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn handlers_see_the_client_address_over_tls() {
    async fn peer(req: Request) -> Response {
        Response::text(format!("{:?}", req.peer_addr()))
    }
    let server = TestServer::start(Server::new(), peer);
    let mut c = server.client();
    let expected = format!("{:?}", Some(c.stream.sock.local_addr().unwrap()));
    c.send("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(c.reply().text(), expected);
}

#[test]
fn the_connection_limit_counts_tls_connections() {
    let server = TestServer::start(Server::new().max_connections(1), echo);
    let mut first = server.client();
    first.send("GET /first HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(first.reply().text(), "GET /first body=");
    drop(first);
    // The slot comes back when the first session ends.
    let mut second = server.client();
    second.send("GET /second HTTP/1.1\r\nHost: localhost\r\n\r\n");
    assert_eq!(second.reply().text(), "GET /second body=");
}
