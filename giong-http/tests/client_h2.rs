//! The HTTP client over HTTP/2: against giong-http's server, through ALPN
//! and with prior knowledge, and against scripted servers that misbehave on
//! purpose, on one giong worker.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream};
use giong::time::sleep;
use giong_http::client::{Client, ErrorKind};
use giong_http::{Request, Response, Server, Version};
use giong_tls::{ktls_stats, Certificates, Ktls, TlsAcceptor, TlsConnector};

// ------------------------------------------------------------------ harness

/// Starts `server` with `handler` on this worker; returns its base URL.
fn serve<H, Fut>(server: Server, handler: H) -> String
where
    H: Fn(Request) -> Fut + 'static,
    Fut: Future<Output = Response> + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move { server.serve(listener, handler).await });
    format!("http://{addr}")
}

/// Starts giong-http's server with TLS; returns its base URL (for
/// `localhost`) and a connector that trusts its certificate.
fn serve_tls<H, Fut>(handler: H) -> (String, TlsConnector)
where
    H: Fn(Request) -> Fut + 'static,
    Fut: Future<Output = Response> + 'static,
{
    serve_tls_with(Ktls::Off, handler)
}

/// [`serve_tls`], with `ktls` for the server's connections.
fn serve_tls_with<H, Fut>(ktls: Ktls, handler: H) -> (String, TlsConnector)
where
    H: Fn(Request) -> Fut + 'static,
    Fut: Future<Output = Response> + 'static,
{
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let (cert, key) = (key.cert.pem(), key.key_pair.serialize_pem());
    let acceptor = TlsAcceptor::with_certificates(
        &Certificates::from_pem(cert.as_bytes(), key.as_bytes()).unwrap(),
    )
    .unwrap();
    let acceptor = acceptor.ktls(ktls).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    giong::spawn(async move { Server::new().serve_tls(listener, acceptor, handler).await });
    (
        format!("https://localhost:{port}"),
        TlsConnector::with_root_pem(cert.as_bytes()).unwrap(),
    )
}

/// A client that speaks HTTP/2 to `http://` URLs.
fn h2c() -> Client {
    Client::builder().http2_prior_knowledge(true).build()
}

/// Waits up to five seconds for `done`.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(5), "{what}");
        sleep(Duration::from_millis(5)).await;
    }
}

fn text_of(size: usize) -> Vec<u8> {
    (0..)
        .flat_map(|i: u32| format!("{{\"id\":{i},\"name\":\"user {}\"}},", i % 1000).into_bytes())
        .take(size)
        .collect()
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    out.extend(miniz_oxide::deflate::compress_to_vec(data, 6));
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xedb8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    out.extend((!crc).to_le_bytes());
    out.extend((data.len() as u32).to_le_bytes());
    out
}

// ------------------------------------------------- a scripted HTTP/2 server

const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const DATA: u8 = 0;
const HEADERS: u8 = 1;
const RST_STREAM: u8 = 3;
const SETTINGS: u8 = 4;
const PING: u8 = 6;
const GOAWAY: u8 = 7;
const WINDOW_UPDATE: u8 = 8;
const END_STREAM: u8 = 1;
const END_HEADERS: u8 = 4;

struct Frame {
    kind: u8,
    flags: u8,
    stream: u32,
    payload: Vec<u8>,
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(9 + payload.len());
    f.extend(&(payload.len() as u32).to_be_bytes()[1..]);
    f.push(kind);
    f.push(flags);
    f.extend(stream.to_be_bytes());
    f.extend(payload);
    f
}

/// A header block of literal fields, never indexed and without Huffman
/// coding, so that it needs no HPACK state.
fn block(fields: &[(&str, &str)]) -> Vec<u8> {
    fn string(b: &mut Vec<u8>, s: &[u8]) {
        let mut n = s.len();
        if n < 127 {
            b.push(n as u8);
        } else {
            b.push(127);
            n -= 127;
            while n >= 128 {
                b.push((n % 128) as u8 | 128);
                n /= 128;
            }
            b.push(n as u8);
        }
        b.extend(s);
    }
    let mut b = Vec::new();
    for (name, value) in fields {
        b.push(0x10);
        string(&mut b, name.as_bytes());
        string(&mut b, value.as_bytes());
    }
    b
}

/// The server's side of an HTTP/2 connection, frame by frame.
struct H2 {
    tcp: TcpStream,
    buf: Vec<u8>,
}

impl H2 {
    async fn fill(&mut self) -> Option<()> {
        let chunk = self.tcp.recv().await.ok()?;
        if chunk.is_empty() {
            return None;
        }
        self.buf.extend_from_slice(&chunk);
        Some(())
    }

    async fn frame(&mut self) -> Option<Frame> {
        loop {
            if self.buf.len() >= 9 {
                let len = u32::from_be_bytes([0, self.buf[0], self.buf[1], self.buf[2]]) as usize;
                if self.buf.len() >= 9 + len {
                    let f = Frame {
                        kind: self.buf[3],
                        flags: self.buf[4],
                        stream: u32::from_be_bytes(self.buf[5..9].try_into().unwrap())
                            & 0x7fff_ffff,
                        payload: self.buf[9..9 + len].to_vec(),
                    };
                    self.buf.drain(..9 + len);
                    return Some(f);
                }
            }
            self.fill().await?;
        }
    }

    /// The stream of the next request, answering SETTINGS and skipping
    /// everything else; `None` once the client has closed the connection.
    async fn request(&mut self) -> Option<u32> {
        loop {
            let f = self.frame().await?;
            match f.kind {
                SETTINGS if f.flags & 1 == 0 => self.send(frame(SETTINGS, 1, 0, &[])).await,
                HEADERS => return Some(f.stream),
                _ => {}
            }
        }
    }

    /// Reads until the client closes the connection.
    async fn drain(mut self) {
        while self.request().await.is_some() {}
    }

    async fn send(&self, bytes: Vec<u8>) {
        let _ = self.tcp.write_all(bytes).await;
    }

    async fn respond(&self, stream: u32, fields: &[(&str, &str)], body: &[u8]) {
        let flags = END_HEADERS | if body.is_empty() { END_STREAM } else { 0 };
        let mut out = frame(HEADERS, flags, stream, &block(fields));
        if !body.is_empty() {
            out.extend(frame(DATA, END_STREAM, stream, body));
        }
        self.send(out).await;
    }

    async fn reset(&self, stream: u32, code: u32) {
        self.send(frame(RST_STREAM, 0, stream, &code.to_be_bytes()))
            .await;
    }

    async fn goaway(&self, last: u32, code: u32) {
        let mut payload = last.to_be_bytes().to_vec();
        payload.extend(code.to_be_bytes());
        self.send(frame(GOAWAY, 0, 0, &payload)).await;
    }
}

/// Starts a server that reads the preface of each connection it accepts,
/// sends its SETTINGS and runs `script` with the connection's number (from
/// 0). Returns its base URL, for a client with prior knowledge.
fn scripted<S, Fut>(script: S) -> String
where
    S: Fn(usize, H2) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let script = Rc::new(script);
    giong::spawn(async move {
        for n in 0.. {
            let Ok(tcp) = listener.accept().await else {
                return;
            };
            let script = script.clone();
            giong::spawn(async move {
                let mut h2 = H2 {
                    tcp,
                    buf: Vec::new(),
                };
                while h2.buf.len() < PREFACE.len() {
                    if h2.fill().await.is_none() {
                        return;
                    }
                }
                assert_eq!(&h2.buf[..PREFACE.len()], PREFACE);
                h2.buf.drain(..PREFACE.len());
                h2.send(frame(SETTINGS, 0, 0, &[])).await;
                script(n, h2).await;
            });
        }
    });
    format!("http://{addr}")
}

const OK: &[(&str, &str)] = &[(":status", "200")];

// ------------------------------------------------- against giong-http's server

#[test]
fn https_chooses_http2_through_alpn_and_requests_share_one_connection() {
    giong::block_on(async {
        let peers = Rc::new(RefCell::new(HashSet::<SocketAddr>::new()));
        let (base, tls) = serve_tls({
            let peers = peers.clone();
            move |req: Request| {
                peers.borrow_mut().insert(req.peer_addr().unwrap());
                async move { Response::text(req.version().as_str()) }
            }
        });
        let client = Client::builder().tls(tls.clone()).build();
        let response = client.get(&base).send().await.unwrap();
        assert_eq!(response.version(), Version::Http2);
        assert_eq!(response.text().unwrap(), "HTTP/2");

        let tasks: Vec<_> = (0..50)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(async move { client.get(&url).send().await.unwrap().into_body() })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap(), b"HTTP/2");
        }
        assert_eq!(peers.borrow().len(), 1);
        assert_eq!(client.connections(&base), (1, 1));

        // Turned off, the client offers HTTP/1.1 alone.
        let client = Client::builder().tls(tls).http2(false).build();
        let response = client.get(&base).send().await.unwrap();
        assert_eq!(response.version(), Version::Http11);
        assert_eq!(response.text().unwrap(), "HTTP/1.1");
    });
}

/// A TLS acceptor for `localhost` and a connector that trusts it.
fn tls_pair() -> (TlsAcceptor, TlsConnector) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let (cert, key) = (key.cert.pem(), key.key_pair.serialize_pem());
    (
        TlsAcceptor::with_certificates(
            &Certificates::from_pem(cert.as_bytes(), key.as_bytes()).unwrap(),
        )
        .unwrap(),
        TlsConnector::with_root_pem(cert.as_bytes()).unwrap(),
    )
}

/// Records the peer address of every request and answers with its version.
fn version_and_peer(
    peers: &Rc<RefCell<HashSet<SocketAddr>>>,
) -> impl Fn(Request) -> std::future::Ready<Response> + 'static {
    let peers = peers.clone();
    move |req: Request| {
        peers.borrow_mut().insert(req.peer_addr().unwrap());
        std::future::ready(Response::text(req.version().as_str()))
    }
}

#[test]
fn a_burst_to_a_new_https_origin_shares_the_first_connection() {
    giong::block_on(async {
        let peers = Rc::new(RefCell::new(HashSet::<SocketAddr>::new()));
        let (base, tls) = serve_tls(version_and_peer(&peers));
        let client = Client::builder().tls(tls).build();
        // All of them ask before the first TLS handshake has finished, so
        // nobody knows yet whether the server speaks HTTP/2.
        let tasks: Vec<_> = (0..50)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(async move { client.get(&url).send().await.unwrap().into_body() })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap(), b"HTTP/2");
        }
        assert_eq!(peers.borrow().len(), 1, "one TCP and TLS connection");
        assert_eq!(client.connections(&base), (1, 1));
    });
}

#[test]
fn a_burst_waiting_for_the_first_connection_opens_its_own_when_alpn_picks_http1() {
    giong::block_on(async {
        let peers = Rc::new(RefCell::new(HashSet::<SocketAddr>::new()));
        let (acceptor, tls) = tls_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let handler = version_and_peer(&peers);
        giong::spawn(async move {
            Server::new()
                .http2(false)
                .serve_tls(listener, acceptor, handler)
                .await
        });
        let client = Client::builder()
            .tls(tls)
            .max_connections_per_host(4)
            .build();
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(async move {
                    let response = client.get(&url).send().await.unwrap();
                    (response.version(), response.into_body())
                })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap(), (Version::Http11, b"HTTP/1.1".to_vec()));
        }
        // Told that the origin speaks HTTP/1.1, the requests in line opened
        // connections of their own instead of queueing behind the first.
        let opened = peers.borrow().len();
        assert!((2..=4).contains(&opened), "{opened} connections");
        assert!(client.connections(&base).0 <= 4);
    });
}

#[test]
fn a_request_waiting_for_a_first_connection_that_is_dropped_opens_one() {
    giong::block_on(async {
        let peers = Rc::new(RefCell::new(HashSet::<SocketAddr>::new()));
        let (acceptor, tls) = tls_pair();
        // Nothing accepts at first: the TLS handshakes wait in the backlog.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let client = Client::builder().tls(tls).build();
        let first = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move {
                client
                    .get(&url)
                    .timeout(Duration::from_millis(100))
                    .send()
                    .await
                    .map(|_| ())
            }
        });
        // Waits for the first one's connection rather than opening its own.
        let second = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move { client.get(&url).send().await.unwrap() }
        });
        let error = first.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Timeout, "{error}");
        let handler = version_and_peer(&peers);
        giong::spawn(async move { Server::new().serve_tls(listener, acceptor, handler).await });
        let response = second.await.unwrap();
        assert_eq!(response.version(), Version::Http2);
        assert_eq!(client.connections(&base), (1, 1));
    });
}

#[test]
fn http2_runs_over_kernel_tls_where_the_kernel_has_it() {
    // Ktls::Auto on both sides: the kernel takes the records where it can,
    // and the connections stay in the process where it cannot, so this runs
    // everywhere and covers kernel TLS where the `tls` module is loaded.
    giong::block_on(async {
        let big = Rc::new(text_of(2 << 20));
        let (base, tls) = serve_tls_with(Ktls::Auto, {
            let big = big.clone();
            move |req: Request| {
                let big = big.clone();
                async move {
                    match req.path() {
                        "/echo" => Response::new(200).body(req.body().to_vec()),
                        _ => Response::new(200).body(big.to_vec()),
                    }
                }
            }
        });
        let tls = tls.ktls(Ktls::Auto).unwrap();
        let in_kernel = tls.ktls_unavailable().is_none();
        let before = ktls_stats().offloaded;
        let client = Client::builder().tls(tls).build();
        // Once the origin is known to speak HTTP/2, the burst shares one
        // connection.
        assert_eq!(client.get(&base).send().await.unwrap().into_body(), *big);
        let upload = text_of(1 << 20);
        let tasks: Vec<_> = (0..8)
            .map(|i| {
                let (client, base, upload) = (client.clone(), base.clone(), upload.clone());
                giong::spawn(async move {
                    let response = if i % 2 == 0 {
                        client
                            .post(&format!("{base}/echo"))
                            .body(upload)
                            .send()
                            .await
                    } else {
                        client.get(&base).send().await
                    };
                    let response = response.unwrap();
                    assert_eq!(response.version(), Version::Http2);
                    response.into_body()
                })
            })
            .collect();
        for (i, t) in tasks.into_iter().enumerate() {
            let body = t.await.unwrap();
            if i % 2 == 0 {
                assert!(body == upload, "echo {i}: {} bytes", body.len());
            } else {
                assert!(body == *big, "download {i}: {} bytes", body.len());
            }
        }
        assert_eq!(client.connections(&base), (1, 1));
        if in_kernel {
            // The client's connection and the server's.
            assert!(ktls_stats().offloaded >= before + 2);
        }
    });
}

#[test]
fn prior_knowledge_speaks_http2_to_a_plain_port() {
    giong::block_on(async {
        let base = serve(Server::new(), |req: Request| async move {
            let host = req.header_str("host").unwrap_or("").to_owned();
            let agent = req.header_str("user-agent").unwrap_or("").to_owned();
            let kind = req.header_str("x-kind").unwrap_or("").to_owned();
            Response::text(format!(
                "{} {} {} host={host} agent={agent} kind={kind} body={}",
                req.version().as_str(),
                req.method(),
                req.target(),
                String::from_utf8_lossy(req.body())
            ))
        });
        let client = h2c();
        let response = client
            .post(&format!("{base}/things?x=1"))
            .header("X-Kind", "test")
            .body("hello")
            .send()
            .await
            .unwrap();
        assert_eq!(response.version(), Version::Http2);
        let addr = base.trim_start_matches("http://");
        assert!(
            response.text().unwrap().starts_with(&format!(
                "HTTP/2 POST /things?x=1 host={addr} agent=giong-http/"
            )),
            "{}",
            response.text().unwrap()
        );
        assert!(response.text().unwrap().ends_with("kind=test body=hello"));

        // A Host header given becomes :authority.
        let text = client
            .get(&base)
            .header("Host", "example.test")
            .send()
            .await
            .unwrap()
            .into_body();
        assert!(String::from_utf8(text)
            .unwrap()
            .contains("host=example.test"));

        let response = client.head(&base).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert!(response.body().is_empty());

        // Without prior knowledge the same port speaks HTTP/1.1.
        let response = Client::new().get(&base).send().await.unwrap();
        assert_eq!(response.version(), Version::Http11);
        assert!(response.text().unwrap().starts_with("HTTP/1.1 GET /"));
    });
}

#[test]
fn bodies_larger_than_every_window_go_both_ways() {
    giong::block_on(async {
        let big = Rc::new(text_of(5 << 20));
        let base = serve(Server::new().max_body_size(8 << 20), {
            let big = big.clone();
            move |req: Request| {
                let big = big.clone();
                async move {
                    match req.path() {
                        "/echo" => Response::new(200).body(req.body().to_vec()),
                        _ => Response::new(200).body(big.to_vec()),
                    }
                }
            }
        });
        let client = h2c();
        let upload = text_of(3 << 20);
        // Several at once, so that the connection's window is shared.
        let tasks: Vec<_> = (0..4)
            .map(|i| {
                let (client, base, upload) = (client.clone(), base.clone(), upload.clone());
                giong::spawn(async move {
                    if i % 2 == 0 {
                        let r = client.post(&format!("{base}/echo")).body(upload).send();
                        r.await.unwrap().into_body()
                    } else {
                        client.get(&base).send().await.unwrap().into_body()
                    }
                })
            })
            .collect();
        for (i, t) in tasks.into_iter().enumerate() {
            let body = t.await.unwrap();
            if i % 2 == 0 {
                assert!(body == upload, "echo {i}: {} bytes", body.len());
            } else {
                assert!(body == *big, "download {i}: {} bytes", body.len());
            }
        }
        assert_eq!(client.connections(&base), (1, 1));

        // Read piece by piece, the body arrives as it is read.
        let mut response = client.get(&base).send_streaming().await.unwrap();
        assert_eq!(response.version(), Version::Http2);
        let mut got = Vec::new();
        let mut pieces = 0;
        while let Some(piece) = response.chunk().await.unwrap() {
            assert!(piece.len() <= 64 * 1024, "{}", piece.len());
            got.extend(piece);
            pieces += 1;
        }
        assert!(got == *big);
        assert!(pieces >= 80, "{pieces}");

        // The limit on what is held applies as it does over HTTP/1.1.
        let e = Client::builder()
            .http2_prior_knowledge(true)
            .max_response_size(1 << 20)
            .build()
            .get(&base)
            .send()
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::TooLarge, "{e}");
    });
}

#[test]
fn requests_wait_for_room_when_the_server_allows_few_streams() {
    giong::block_on(async {
        let peers = Rc::new(RefCell::new(HashSet::<SocketAddr>::new()));
        let in_flight = Rc::new(Cell::new(0));
        let peak = Rc::new(Cell::new(0));
        let base = serve(Server::new().max_concurrent_streams(2), {
            let (peers, in_flight, peak) = (peers.clone(), in_flight.clone(), peak.clone());
            move |req: Request| {
                let (peers, in_flight, peak) = (peers.clone(), in_flight.clone(), peak.clone());
                async move {
                    peers.borrow_mut().insert(req.peer_addr().unwrap());
                    in_flight.set(in_flight.get() + 1);
                    peak.set(peak.get().max(in_flight.get()));
                    sleep(Duration::from_millis(10)).await;
                    in_flight.set(in_flight.get() - 1);
                    Response::text("ok")
                }
            }
        });
        let client = Client::builder()
            .http2_prior_knowledge(true)
            .max_connections_per_host(1)
            .build();
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(async move { client.get(&url).send().await.map(|r| r.status()) })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap().unwrap(), 200);
        }
        assert_eq!(peers.borrow().len(), 1);
        assert_eq!(peak.get(), 2);
        assert_eq!(client.connections(&base), (1, 1));

        // With room for more connections, more are opened.
        peers.borrow_mut().clear();
        let client = Client::builder()
            .http2_prior_knowledge(true)
            .max_connections_per_host(3)
            .build();
        // The first connection is opened, and its SETTINGS read, first.
        client.get(&base).send().await.unwrap();
        let tasks: Vec<_> = (0..20)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(async move { client.get(&url).send().await.map(|r| r.status()) })
            })
            .collect();
        for t in tasks {
            assert_eq!(t.await.unwrap().unwrap(), 200);
        }
        assert_eq!(peers.borrow().len(), 3);
        assert_eq!(client.connections(&base), (3, 3));
    });
}

#[test]
fn a_dropped_request_resets_its_stream_and_the_connection_carries_on() {
    giong::block_on(async {
        struct Dropped(Rc<Cell<bool>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let dropped = Rc::new(Cell::new(false));
        let peers = Rc::new(RefCell::new(HashSet::<SocketAddr>::new()));
        let base = serve(Server::new(), {
            let (dropped, peers) = (dropped.clone(), peers.clone());
            move |req: Request| {
                peers.borrow_mut().insert(req.peer_addr().unwrap());
                let guard = Dropped(dropped.clone());
                async move {
                    if req.path() == "/slow" {
                        sleep(Duration::from_secs(30)).await;
                    }
                    drop(guard);
                    Response::text("ok")
                }
            }
        });
        let client = h2c();
        client.get(&base).send().await.unwrap();
        dropped.set(false);
        let slow = client.get(&format!("{base}/slow")).send();
        assert!(giong::time::timeout(Duration::from_millis(50), slow)
            .await
            .is_err());
        // The server dropped the handler when the stream was reset.
        eventually("the handler is dropped", || dropped.get()).await;

        let e = client
            .get(&format!("{base}/slow"))
            .timeout(Duration::from_millis(50))
            .send()
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Timeout, "{e}");

        assert_eq!(client.connections(&base), (1, 1));
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
        assert_eq!(peers.borrow().len(), 1);

        // So does a streamed response dropped before its end.
        let big = client.get(&format!("{base}/big")).send_streaming().await;
        drop(big.unwrap());
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
        assert_eq!(peers.borrow().len(), 1);
    });
}

#[test]
fn redirects_and_compressed_bodies_work_as_over_http1() {
    giong::block_on(async {
        let page = Rc::new(text_of(200_000));
        let base = serve(Server::new(), {
            let page = page.clone();
            move |req: Request| {
                let page = page.clone();
                async move {
                    match req.path() {
                        "/old" => Response::new(301).header("Location", "/new"),
                        "/new" => Response::text("moved"),
                        "/gz" => Response::new(200)
                            .header("Content-Encoding", "gzip")
                            .body(gzip(&page)),
                        _ => Response::new(404),
                    }
                }
            }
        });
        let client = h2c();
        let response = client.get(&format!("{base}/old")).send().await.unwrap();
        assert_eq!(response.text().unwrap(), "moved");
        assert!(response.url().ends_with("/new"), "{}", response.url());

        let response = client.get(&format!("{base}/gz")).send().await.unwrap();
        assert_eq!(response.header("content-encoding"), None);
        assert!(response.body() == page.as_slice());

        let mut response = client
            .get(&format!("{base}/gz"))
            .send_streaming()
            .await
            .unwrap();
        let mut got = Vec::new();
        while let Some(piece) = response.chunk().await.unwrap() {
            got.extend(piece);
        }
        assert!(got == *page);

        let e = client
            .get(&format!("{base}/missing"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap_err();
        assert_eq!(e.status(), Some(404));
        assert_eq!(client.connections(&base), (1, 1));
    });
}

#[test]
fn idle_connections_are_closed_by_either_side() {
    giong::block_on(async {
        let base = serve(Server::new(), |_req: Request| async {
            Response::text("ok")
        });
        let client = Client::builder()
            .http2_prior_knowledge(true)
            .idle_timeout(Duration::from_millis(100))
            .build();
        client.get(&base).send().await.unwrap();
        assert_eq!(client.connections(&base), (1, 1));
        eventually("the client closes its idle connection", || {
            client.connections(&base) == (0, 0)
        })
        .await;

        let base = serve(
            Server::new().idle_timeout(Duration::from_millis(100)),
            |_req: Request| async { Response::text("ok") },
        );
        let client = h2c();
        client.get(&base).send().await.unwrap();
        eventually("the server's GOAWAY closes the connection", || {
            client.connections(&base) == (0, 0)
        })
        .await;
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
    });
}

// ------------------------------------------------- against scripted servers

#[test]
fn refused_requests_are_sent_again_whatever_their_method() {
    giong::block_on(async {
        let requests = Rc::new(Cell::new(0));
        let base = scripted({
            let requests = requests.clone();
            move |n, mut h2| {
                let requests = requests.clone();
                async move {
                    while let Some(stream) = h2.request().await {
                        requests.set(requests.get() + 1);
                        match (n, requests.get()) {
                            // REFUSED_STREAM: not processed.
                            (0, 1) => h2.reset(stream, 0x7).await,
                            (0, 2) => h2.respond(stream, OK, b"second try").await,
                            // A GOAWAY that leaves the stream out.
                            (0, 3) => {
                                h2.goaway(stream - 2, 0).await;
                                return h2.drain().await;
                            }
                            (_, _) => h2.respond(stream, OK, b"new connection").await,
                        }
                    }
                }
            }
        });
        let client = h2c();
        let response = client.post(&base).body("x").send().await.unwrap();
        assert_eq!(response.text().unwrap(), "second try");
        let response = client.post(&base).body("y").send().await.unwrap();
        assert_eq!(response.text().unwrap(), "new connection");
        assert_eq!(requests.get(), 4);
    });
}

#[test]
fn a_reset_stream_fails_its_request_alone() {
    giong::block_on(async {
        let connections = Rc::new(Cell::new(0));
        let base = scripted({
            let connections = connections.clone();
            move |_, mut h2| {
                connections.set(connections.get() + 1);
                async move {
                    let mut i = 0;
                    while let Some(stream) = h2.request().await {
                        i += 1;
                        if i == 1 {
                            h2.reset(stream, 0x2).await; // INTERNAL_ERROR
                        } else {
                            h2.respond(stream, OK, b"fine").await;
                        }
                    }
                }
            }
        });
        let client = h2c();
        let e = client.get(&base).send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        assert!(e.to_string().contains("INTERNAL_ERROR"), "{e}");
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "fine"
        );
        assert_eq!(connections.get(), 1);
    });
}

#[test]
fn malformed_responses_are_protocol_errors() {
    giong::block_on(async {
        let answer = Rc::new(Cell::new(0));
        let base = scripted({
            let answer = answer.clone();
            move |_, mut h2| {
                let answer = answer.clone();
                async move {
                    while let Some(stream) = h2.request().await {
                        answer.set(answer.get() + 1);
                        match answer.get() {
                            1 => {
                                let head = [(":status", "200"), ("Content-Type", "text/plain")];
                                h2.respond(stream, &head, b"").await;
                            }
                            2 => {
                                let head = [(":status", "200"), ("connection", "keep-alive")];
                                h2.respond(stream, &head, b"").await;
                            }
                            3 => h2.respond(stream, &[("server", "x")], b"").await,
                            4 => {
                                let head = [(":status", "200"), ("content-length", "5")];
                                h2.respond(stream, &head, b"abc").await;
                            }
                            5 => {
                                let head = [("server", "x"), (":status", "200")];
                                h2.respond(stream, &head, b"").await;
                            }
                            _ => h2.respond(stream, OK, b"good").await,
                        }
                    }
                }
            }
        });
        let client = h2c();
        for what in [
            "uppercase name",
            "connection field",
            "no :status",
            "short body",
            "pseudo after regular",
        ] {
            let e = client.get(&base).send().await.unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Protocol, "{what}: {e}");
            assert!(e.to_string().contains("HTTP/2"), "{what}: {e}");
        }
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "good"
        );
    });
}

#[test]
fn a_connection_closed_before_answering_is_retried_only_when_safe() {
    giong::block_on(async {
        let base = scripted(|n, mut h2| async move {
            // Each connection answers one request, then closes when the
            // next arrives.
            let Some(stream) = h2.request().await else {
                return;
            };
            h2.respond(stream, OK, format!("connection {n}").as_bytes())
                .await;
            h2.request().await;
        });
        let client = h2c();
        let get = |client: &Client| {
            let r = client.get(&base).send();
            async move { r.await.unwrap().into_body() }
        };
        assert_eq!(get(&client).await, b"connection 0");
        // Sent on the used connection, which closes: a GET goes again.
        assert_eq!(get(&client).await, b"connection 1");
        // A POST does not.
        let e = client.post(&base).body("once").send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        assert!(e.to_string().contains("not sent again"), "{e}");

        // The head arrived, the body did not: no retry for any method.
        let base = scripted(|_, mut h2| async move {
            let Some(stream) = h2.request().await else {
                return;
            };
            let head = block(&[(":status", "200"), ("content-length", "10")]);
            h2.send(frame(HEADERS, END_HEADERS, stream, &head)).await;
            h2.send(frame(DATA, 0, stream, b"abc")).await;
        });
        let e = h2c().get(&base).send().await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
    });
}

#[test]
fn a_window_read_after_the_connection_went_quiet_is_granted_again() {
    // The server fills the stream's window and waits for more of it. The
    // program reads the body only once nothing else is happening on the
    // connection, so the WINDOW_UPDATE that reading queues must wake the
    // connection's task to be sent.
    giong::block_on(async {
        let sent = Rc::new(Cell::new(false));
        let base = scripted({
            let sent = sent.clone();
            move |_, mut h2| {
                let sent = sent.clone();
                async move {
                    let Some(stream) = h2.request().await else {
                        return;
                    };
                    h2.send(frame(HEADERS, END_HEADERS, stream, &block(OK)))
                        .await;
                    let piece = vec![b'x'; 16384];
                    for _ in 0..64 {
                        h2.send(frame(DATA, 0, stream, &piece)).await;
                    }
                    sent.set(true);
                    while let Some(f) = h2.frame().await {
                        if f.kind == WINDOW_UPDATE && f.stream == stream {
                            h2.send(frame(DATA, END_STREAM, stream, &piece)).await;
                            break;
                        }
                    }
                    h2.drain().await;
                }
            }
        });
        let client = h2c();
        let mut response = client.get(&base).send_streaming().await.unwrap();
        eventually("the server sent a window's worth", || sent.get()).await;
        sleep(Duration::from_millis(50)).await;
        let read = async {
            let mut got = 0;
            while let Some(piece) = response.chunk().await.unwrap() {
                got += piece.len();
            }
            got
        };
        let got = giong::time::timeout(Duration::from_secs(5), read).await;
        assert_eq!(got.expect("the body stalled"), 65 * 16384);
    });
}

#[test]
fn a_server_that_sends_beyond_the_window_loses_the_stream() {
    giong::block_on(async {
        let sent = Rc::new(Cell::new(false));
        let base = scripted({
            let sent = sent.clone();
            move |_, mut h2| {
                let sent = sent.clone();
                async move {
                    let Some(stream) = h2.request().await else {
                        return;
                    };
                    h2.send(frame(HEADERS, END_HEADERS, stream, &block(OK)))
                        .await;
                    // 1 MiB is what the client allows on a stream until it
                    // reads; this is one frame more.
                    let piece = vec![b'x'; 16384];
                    for _ in 0..65 {
                        h2.send(frame(DATA, 0, stream, &piece)).await;
                    }
                    sent.set(true);
                    h2.drain().await;
                }
            }
        });
        let client = h2c();
        let mut response = client.get(&base).send_streaming().await.unwrap();
        eventually("the server sent it all", || sent.get()).await;
        sleep(Duration::from_millis(50)).await;
        let mut got = 0;
        let e = loop {
            match response.chunk().await {
                Ok(Some(piece)) => got += piece.len(),
                Ok(None) => panic!("the body ended after {got} bytes"),
                Err(e) => break e,
            }
        };
        assert_eq!(e.kind(), ErrorKind::Protocol, "{e}");
        assert!(got <= 1 << 20, "{got}");
    });
}

#[test]
fn dropping_the_client_closes_its_connections() {
    giong::block_on(async {
        let closed = Rc::new(Cell::new(false));
        let base = scripted({
            let closed = closed.clone();
            move |_, mut h2| {
                let closed = closed.clone();
                async move {
                    while let Some(stream) = h2.request().await {
                        h2.respond(stream, OK, b"ok").await;
                    }
                    closed.set(true);
                }
            }
        });
        let client = h2c();
        let other = client.clone();
        client.get(&base).send().await.unwrap();
        drop(client);
        // A clone keeps the connection.
        sleep(Duration::from_millis(50)).await;
        assert!(!closed.get());
        assert_eq!(other.get(&base).send().await.unwrap().text().unwrap(), "ok");
        drop(other);
        eventually("the connection is closed", || closed.get()).await;
    });
}

// ------------------------------------------------------------- preconnect

/// Counts the requests a server answers, and the TCP connections they came
/// on.
#[derive(Clone, Default)]
struct Seen {
    peers: Rc<RefCell<HashSet<SocketAddr>>>,
    requests: Rc<Cell<usize>>,
}

impl Seen {
    fn handler(&self) -> impl Fn(Request) -> std::future::Ready<Response> + 'static {
        let seen = self.clone();
        move |req: Request| {
            seen.peers.borrow_mut().insert(req.peer_addr().unwrap());
            seen.requests.set(seen.requests.get() + 1);
            std::future::ready(Response::text(req.version().as_str()))
        }
    }

    fn connections(&self) -> usize {
        self.peers.borrow().len()
    }
}

/// Starts `count` GET requests to `url`, each in a task of its own.
fn burst(client: &Client, url: &str, count: usize) -> Vec<giong::JoinHandle<(Version, Vec<u8>)>> {
    (0..count)
        .map(|_| {
            let (client, url) = (client.clone(), url.to_owned());
            giong::spawn(async move {
                let response = client.get(&url).send().await.unwrap();
                (response.version(), response.into_body())
            })
        })
        .collect()
}

#[test]
fn preconnect_opens_one_http2_connection_that_requests_join_and_sends_no_request() {
    giong::block_on(async {
        let seen = Seen::default();
        let (base, tls) = serve_tls(seen.handler());
        let client = Client::builder().tls(tls).build();
        let warmup = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move { client.preconnect(&url).await }
        });
        // Asked while the handshake is going on: they wait for it.
        let tasks = burst(&client, &format!("{base}/chat"), 4);
        assert_eq!(warmup.await.unwrap().unwrap(), Version::Http2);
        for t in tasks {
            assert_eq!(t.await.unwrap(), (Version::Http2, b"HTTP/2".to_vec()));
        }
        assert_eq!(seen.connections(), 1, "one TCP and TLS connection");
        assert_eq!(seen.requests.get(), 4, "no request of the warm-up's own");

        // Warm already: no other connection.
        assert_eq!(client.preconnect(&base).await.unwrap(), Version::Http2);
        assert_eq!(client.connections(&base), (1, 1));

        // Alone, it opens the connection and the request finds it.
        let seen = Seen::default();
        let (base, tls) = serve_tls(seen.handler());
        let client = Client::builder().tls(tls).build();
        assert_eq!(client.preconnect(&base).await.unwrap(), Version::Http2);
        assert_eq!(client.connections(&base), (1, 1));
        assert_eq!(seen.requests.get(), 0);
        client.get(&base).send().await.unwrap();
        assert_eq!((seen.connections(), seen.requests.get()), (1, 1));
    });
}

#[test]
fn preconnect_parks_the_connection_when_alpn_picks_http1() {
    giong::block_on(async {
        let seen = Seen::default();
        let (acceptor, tls) = tls_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let handler = seen.handler();
        giong::spawn(async move {
            Server::new()
                .http2(false)
                .serve_tls(listener, acceptor, handler)
                .await
        });
        let client = Client::builder()
            .tls(tls)
            .max_connections_per_host(4)
            .build();
        let warmup = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move { client.preconnect(&url).await }
        });
        let tasks = burst(&client, &base, 8);
        assert_eq!(warmup.await.unwrap().unwrap(), Version::Http11);
        for t in tasks {
            assert_eq!(t.await.unwrap(), (Version::Http11, b"HTTP/1.1".to_vec()));
        }
        assert!(
            seen.connections() <= 4,
            "{} connections",
            seen.connections()
        );
        assert_eq!(seen.requests.get(), 8);

        // Alone, the connection waits among the idle ones for the next
        // request.
        let seen = Seen::default();
        let (acceptor, tls) = tls_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let handler = seen.handler();
        giong::spawn(async move {
            Server::new()
                .http2(false)
                .serve_tls(listener, acceptor, handler)
                .await
        });
        let client = Client::builder().tls(tls).build();
        assert_eq!(client.preconnect(&base).await.unwrap(), Version::Http11);
        assert_eq!(client.connections(&base), (1, 1));
        assert_eq!(client.preconnect(&base).await.unwrap(), Version::Http11);
        assert_eq!(client.connections(&base), (1, 1), "the idle one is enough");
        client.get(&base).send().await.unwrap();
        assert_eq!((seen.connections(), seen.requests.get()), (1, 1));
    });
}

/// Passes connections on to `to`, except the first, which it closes at once.
fn closes_the_first_connection(to: SocketAddr) -> u16 {
    relay(to, true).0
}

/// Passes connections on to `to` (closing the first at once if
/// `drop_first`); returns its port and how many connections it accepted.
fn relay(to: SocketAddr, drop_first: bool) -> (u16, Rc<Cell<usize>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Rc::new(Cell::new(0));
    let count = accepted.clone();
    giong::spawn(async move {
        for n in 0.. {
            let Ok(client) = listener.accept().await else {
                return;
            };
            count.set(count.get() + 1);
            if n == 0 && drop_first {
                drop(client);
                continue;
            }
            let server = TcpStream::connect(to).await.unwrap();
            let (client, server) = (Rc::new(client), Rc::new(server));
            for (from, to) in [(client.clone(), server.clone()), (server, client)] {
                giong::spawn(async move {
                    while let Ok(chunk) = from.recv().await {
                        if chunk.is_empty() || to.write_all(chunk.to_vec()).await.0.is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown(std::net::Shutdown::Write);
                });
            }
        }
    });
    (port, accepted)
}

#[test]
fn a_failed_preconnect_hands_its_place_to_the_requests_waiting() {
    giong::block_on(async {
        let seen = Seen::default();
        let (acceptor, tls) = tls_pair();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = listener.local_addr().unwrap();
        let handler = seen.handler();
        giong::spawn(async move { Server::new().serve_tls(listener, acceptor, handler).await });
        let base = format!("https://localhost:{}", closes_the_first_connection(server));
        let client = Client::builder().tls(tls).build();
        let warmup = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move { client.preconnect(&url).await }
        });
        let tasks = burst(&client, &base, 3);
        let e = warmup.await.unwrap().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        for t in tasks {
            assert_eq!(t.await.unwrap(), (Version::Http2, b"HTTP/2".to_vec()));
        }
        assert_eq!((seen.connections(), seen.requests.get()), (1, 3));
        assert_eq!(client.connections(&base), (1, 1));
    });
}

#[test]
fn a_cancelled_preconnect_hands_its_place_to_the_requests_waiting() {
    giong::block_on(async {
        let seen = Seen::default();
        let (acceptor, tls) = tls_pair();
        // Nothing accepts at first: the TLS handshake waits in the backlog.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let client = Client::builder().tls(tls).build();
        let warmup = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move {
                giong::time::timeout(Duration::from_millis(100), client.preconnect(&url)).await
            }
        });
        let tasks = burst(&client, &base, 3);
        assert!(warmup.await.unwrap().is_err(), "the warm-up was cut short");
        let handler = seen.handler();
        giong::spawn(async move { Server::new().serve_tls(listener, acceptor, handler).await });
        for t in tasks {
            assert_eq!(t.await.unwrap(), (Version::Http2, b"HTTP/2".to_vec()));
        }
        assert_eq!((seen.connections(), seen.requests.get()), (1, 3));
        assert_eq!(client.connections(&base), (1, 1));
    });
}

#[test]
fn preconnect_reports_a_bad_url_and_a_server_that_is_not_there() {
    giong::block_on(async {
        let client = Client::new();
        let e = client.preconnect("ftp://example.com/").await.unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Request, "{e}");
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let e = client
            .preconnect(&format!("http://127.0.0.1:{port}"))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Connect, "{e}");
        assert_eq!(
            client.connections(&format!("http://127.0.0.1:{port}")),
            (0, 0)
        );
    });
}

// ------------------------------------------------------- PINGs when idle

#[test]
fn an_acknowledged_ping_keeps_the_connection_and_its_idle_time_starts_again() {
    giong::block_on(async {
        let seen = Seen::default();
        let (base, tls) = serve_tls(seen.handler());
        let client = Client::builder()
            .tls(tls)
            .idle_timeout(Duration::from_secs(2))
            .build();
        assert_eq!(
            client
                .ping_idle_http2(&base, Duration::from_secs(5))
                .await
                .unwrap(),
            0,
            "nothing to ping yet"
        );
        client.get(&base).send().await.unwrap();
        sleep(Duration::from_millis(1400)).await;
        assert_eq!(
            client
                .ping_idle_http2(&base, Duration::from_secs(5))
                .await
                .unwrap(),
            1
        );
        // Past the idle timeout counted from the request, not from the PING.
        sleep(Duration::from_millis(1400)).await;
        assert_eq!(client.connections(&base), (1, 1));
        client.get(&base).send().await.unwrap();
        assert_eq!((seen.connections(), seen.requests.get()), (1, 2));

        let e = client
            .ping_idle_http2(&base, Duration::ZERO)
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Request, "{e}");
    });
}

/// The server's side of a connection that answers requests and does with
/// the client's PINGs what `on_ping` says; returns the requests it saw.
async fn answer_requests(mut h2: H2, on_ping: impl Fn(&[u8]) -> Option<Vec<u8>>) -> usize {
    let mut requests = 0;
    while let Some(f) = h2.frame().await {
        match f.kind {
            SETTINGS if f.flags & 1 == 0 => h2.send(frame(SETTINGS, 1, 0, &[])).await,
            HEADERS => {
                requests += 1;
                h2.respond(f.stream, OK, b"ok").await;
            }
            PING if f.flags & 1 == 0 => {
                if let Some(reply) = on_ping(&f.payload) {
                    h2.send(reply).await;
                }
            }
            _ => {}
        }
    }
    requests
}

#[test]
fn a_ping_left_unanswered_closes_the_idle_connection() {
    giong::block_on(async {
        let requests = Rc::new(RefCell::new(Vec::new()));
        let base = scripted({
            let requests = requests.clone();
            move |n, h2| {
                let requests = requests.clone();
                async move {
                    let seen = answer_requests(h2, move |payload| match n {
                        // Silent, then an acknowledgement of another PING.
                        0 => {
                            let mut other = payload.to_vec();
                            other[7] ^= 0xff;
                            Some(frame(PING, 1, 0, &other))
                        }
                        _ => Some(frame(PING, 1, 0, payload)),
                    })
                    .await;
                    requests.borrow_mut().push((n, seen));
                }
            }
        });
        let client = h2c();
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
        let e = client
            .ping_idle_http2(&base, Duration::from_millis(150))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Timeout, "{e}");
        assert!(e.to_string().contains("did not acknowledge"), "{e}");
        // Gone at once: the next request opens a new connection.
        assert_eq!(client.connections(&base), (0, 0));
        assert_eq!(client.preconnect(&base).await.unwrap(), Version::Http2);
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
        assert_eq!(
            client
                .ping_idle_http2(&base, Duration::from_secs(5))
                .await
                .unwrap(),
            1
        );
        drop(client);
        eventually("both connections end", || requests.borrow().len() == 2).await;
        let mut requests = requests.borrow().clone();
        requests.sort_unstable();
        assert_eq!(requests, [(0, 1), (1, 1)], "one request on each");
    });
}

#[test]
fn a_request_started_during_a_ping_keeps_its_connection() {
    giong::block_on(async {
        let base = scripted(|_, mut h2| async move {
            // Never acknowledges a PING; answers the second request late.
            let mut requests = 0;
            while let Some(stream) = h2.request().await {
                requests += 1;
                if requests == 2 {
                    sleep(Duration::from_millis(1000)).await;
                }
                h2.respond(stream, OK, b"ok").await;
            }
        });
        let client = h2c();
        client.get(&base).send().await.unwrap();
        let probe = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move {
                client
                    .ping_idle_http2(&url, Duration::from_millis(500))
                    .await
            }
        });
        sleep(Duration::from_millis(20)).await;
        let request = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move { client.get(&url).send().await.map(|r| r.into_body()) }
        });
        let e = probe.await.unwrap().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Timeout, "{e}");
        assert!(e.to_string().contains("1 left open"), "{e}");
        assert_eq!(client.connections(&base), (1, 0), "left to the request");
        assert_eq!(request.await.unwrap().unwrap(), b"ok");
        assert_eq!(client.connections(&base), (1, 1));
    });
}

#[test]
fn a_goaway_during_a_ping_is_reported_and_the_connection_leaves_the_pool() {
    giong::block_on(async {
        let base = scripted(|n, h2| async move {
            answer_requests(h2, move |_| {
                (n == 0).then(|| {
                    let mut payload = 1u32.to_be_bytes().to_vec();
                    payload.extend(0u32.to_be_bytes());
                    frame(GOAWAY, 0, 0, &payload)
                })
            })
            .await;
        });
        let client = h2c();
        client.get(&base).send().await.unwrap();
        let e = client
            .ping_idle_http2(&base, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        eventually("the connection leaves the pool", || {
            client.connections(&base) == (0, 0)
        })
        .await;
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
    });
}

// ------------------------------------------------- races, to harden both

/// giong-http's TLS server for `localhost` behind a relay that counts the
/// connections dialled; `http2` off makes ALPN pick HTTP/1.1. Returns its
/// base URL, a connector that trusts it, and the count.
fn counted_tls(seen: &Seen, http2: bool) -> (String, TlsConnector, Rc<Cell<usize>>) {
    let (acceptor, tls) = tls_pair();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = listener.local_addr().unwrap();
    let handler = seen.handler();
    giong::spawn(async move {
        Server::new()
            .http2(http2)
            .serve_tls(listener, acceptor, handler)
            .await
    });
    let (port, dials) = relay(server, false);
    (format!("https://localhost:{port}"), tls, dials)
}

#[test]
fn concurrent_preconnects_dial_once() {
    giong::block_on(async {
        let seen = Seen::default();
        let (base, tls, dials) = counted_tls(&seen, true);
        let client = Client::builder().tls(tls).build();
        let warmups: Vec<_> = (0..5)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(async move { client.preconnect(&url).await.unwrap() })
            })
            .collect();
        for w in warmups {
            assert_eq!(w.await.unwrap(), Version::Http2);
        }
        assert_eq!(dials.get(), 1);
        assert_eq!(client.connections(&base), (1, 1));
        assert_eq!(seen.requests.get(), 0);
        for t in burst(&client, &base, 10) {
            assert_eq!(t.await.unwrap().0, Version::Http2);
        }
        assert_eq!((dials.get(), seen.connections()), (1, 1));
    });
}

#[test]
fn a_request_given_up_while_waiting_for_a_preconnect_leaves_the_others_alone() {
    giong::block_on(async {
        let seen = Seen::default();
        let (base, tls, dials) = counted_tls(&seen, true);
        let client = Client::builder().tls(tls).build();
        let warmup = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move { client.preconnect(&url).await }
        });
        // In line behind the preconnect, then given up.
        let impatient = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move { polled_once_then_dropped(client.get(&url).send()).await }
        });
        let tasks = burst(&client, &base, 3);
        assert!(impatient.await.unwrap(), "it was still waiting");
        assert_eq!(warmup.await.unwrap().unwrap(), Version::Http2);
        for t in tasks {
            assert_eq!(t.await.unwrap().0, Version::Http2);
        }
        assert_eq!(dials.get(), 1);
        assert_eq!(seen.requests.get(), 3);
        assert_eq!(client.connections(&base), (1, 1));
    });
}

/// Polls `fut` once and drops it: returns whether it was still pending,
/// which a timeout of a microsecond cannot promise (what it waits for may
/// arrive before the timer is looked at).
async fn polled_once_then_dropped<F: Future>(fut: F) -> bool {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| std::task::Poll::Ready(fut.as_mut().poll(cx).is_pending())).await
}

/// A small deterministic generator, so that a failing round can be named
/// and replayed.
struct XorShift(u64);

impl XorShift {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[test]
fn preconnects_and_requests_given_up_at_random_never_wedge_the_pool() {
    giong::block_on(async {
        let h2_seen = Seen::default();
        let h1_seen = Seen::default();
        let (h2_base, h2_tls, _) = counted_tls(&h2_seen, true);
        let (h1_base, h1_tls, _) = counted_tls(&h1_seen, false);
        for round in 0..40u64 {
            let mut rng = XorShift(0x9e37_79b9_7f4a_7c15 ^ (round + 1));
            let http2 = round % 2 == 0;
            let (base, tls, version) = if http2 {
                (h2_base.clone(), h2_tls.clone(), Version::Http2)
            } else {
                (h1_base.clone(), h1_tls.clone(), Version::Http11)
            };
            let max = 1 + rng.below(4) as usize;
            let client = Client::builder()
                .tls(tls)
                .max_connections_per_host(max)
                .build();
            let mut tasks = Vec::new();
            // Preconnects and requests, in a random order, some given up
            // after a random few milliseconds: while dialling, waiting in
            // line or waiting for their response.
            for _ in 0..2 + rng.below(10) {
                let (client, url) = (client.clone(), base.clone());
                let preconnect = rng.below(3) == 0;
                let give_up = (rng.below(3) == 0).then(|| Duration::from_millis(rng.below(20)));
                let delay = Duration::from_millis(rng.below(5));
                tasks.push(giong::spawn(async move {
                    sleep(delay).await;
                    let work = async {
                        if preconnect {
                            client.preconnect(&url).await
                        } else {
                            client.get(&url).send().await.map(|r| r.version())
                        }
                    };
                    match give_up {
                        Some(after) => giong::time::timeout(after, work).await.ok(),
                        None => Some(work.await),
                    }
                }));
            }
            for t in tasks {
                if let Some(result) = t.await.unwrap() {
                    let got = result.unwrap_or_else(|e| panic!("round {round}: {e}"));
                    assert_eq!(got, version, "round {round}");
                }
            }
            // Nothing is left holding a place, and the pool still serves.
            let response = client
                .get(&base)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .unwrap_or_else(|e| panic!("round {round}: the pool is stuck: {e}"));
            assert_eq!(response.version(), version, "round {round}");
            eventually(&format!("round {round}: every connection idle"), || {
                let (open, idle) = client.connections(&base);
                open == idle && open >= 1 && open <= max
            })
            .await;
            if http2 {
                assert_eq!(client.connections(&base), (1, 1), "round {round}");
            }
        }
    });
}

#[test]
fn probes_at_once_each_get_their_acknowledgement() {
    giong::block_on(async {
        let seen = Seen::default();
        let (base, tls) = serve_tls(seen.handler());
        let client = Client::builder().tls(tls).build();
        client.preconnect(&base).await.unwrap();
        let probes: Vec<_> = (0..3)
            .map(|_| {
                let (client, url) = (client.clone(), base.clone());
                giong::spawn(
                    async move { client.ping_idle_http2(&url, Duration::from_secs(5)).await },
                )
            })
            .collect();
        for p in probes {
            assert_eq!(p.await.unwrap().unwrap(), 1);
        }
        assert_eq!(client.connections(&base), (1, 1));
        // A probe given up early closes nothing.
        assert!(
            polled_once_then_dropped(client.ping_idle_http2(&base, Duration::from_secs(5))).await
        );
        assert_eq!(client.connections(&base), (1, 1));
        assert_eq!(
            client
                .ping_idle_http2(&base, Duration::from_secs(5))
                .await
                .unwrap(),
            1
        );
    });
}

#[test]
fn a_server_ping_with_the_same_payload_is_not_an_acknowledgement() {
    giong::block_on(async {
        let answered = Rc::new(Cell::new(false));
        let base = scripted({
            let answered = answered.clone();
            move |_, mut h2| {
                let answered = answered.clone();
                async move {
                    let mut ours: Option<Vec<u8>> = None;
                    while let Some(f) = h2.frame().await {
                        match f.kind {
                            SETTINGS if f.flags & 1 == 0 => {
                                h2.send(frame(SETTINGS, 1, 0, &[])).await
                            }
                            HEADERS => h2.respond(f.stream, OK, b"ok").await,
                            // The client's PING: echoed as a PING of the
                            // server's own, not as an acknowledgement.
                            PING if f.flags & 1 == 0 && ours.is_none() => {
                                ours = Some(f.payload.clone());
                                h2.send(frame(PING, 0, 0, &f.payload)).await;
                            }
                            // The client acknowledges the server's PING.
                            PING if ours.as_ref() == Some(&f.payload) => answered.set(true),
                            _ => {}
                        }
                    }
                }
            }
        });
        let client = h2c();
        client.get(&base).send().await.unwrap();
        let e = client
            .ping_idle_http2(&base, Duration::from_millis(150))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Timeout, "{e}");
        assert!(answered.get(), "the client answered the server's PING");
        assert_eq!(client.connections(&base), (0, 0));
    });
}

#[test]
fn probing_an_origin_that_speaks_http1_finds_nothing_to_ping() {
    giong::block_on(async {
        let base = serve(Server::new(), |_req: Request| async {
            Response::text("ok")
        });
        let client = Client::new();
        client.get(&base).send().await.unwrap();
        assert_eq!(
            client
                .ping_idle_http2(&base, Duration::from_secs(5))
                .await
                .unwrap(),
            0
        );
        assert_eq!(client.connections(&base), (1, 1), "left as it was");
        let e = client
            .ping_idle_http2("not a url", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Request, "{e}");
    });
}

// ------------------------------------------- review of #184: regressions

#[test]
fn the_idle_timeout_does_not_end_a_connection_while_its_ping_is_out() {
    giong::block_on(async {
        let base = scripted(|_, mut h2| async move {
            while let Some(f) = h2.frame().await {
                match f.kind {
                    SETTINGS if f.flags & 1 == 0 => h2.send(frame(SETTINGS, 1, 0, &[])).await,
                    HEADERS => h2.respond(f.stream, OK, b"ok").await,
                    // A slow acknowledgement, which arrives after the
                    // client's idle deadline.
                    PING if f.flags & 1 == 0 => {
                        sleep(Duration::from_millis(300)).await;
                        h2.send(frame(PING, 1, 0, &f.payload)).await;
                    }
                    _ => {}
                }
            }
        });
        let client = Client::builder()
            .http2_prior_knowledge(true)
            .idle_timeout(Duration::from_millis(300))
            .build();
        client.get(&base).send().await.unwrap();
        sleep(Duration::from_millis(100)).await;
        // The idle deadline passes about 200 ms into the probe.
        assert_eq!(
            client
                .ping_idle_http2(&base, Duration::from_secs(5))
                .await
                .unwrap(),
            1
        );
        assert_eq!(client.connections(&base), (1, 1));
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
        // Once the probe is over, the idle timeout applies again.
        eventually("the idle connection is closed", || {
            client.connections(&base) == (0, 0)
        })
        .await;
    });
}

#[test]
fn a_connection_that_answered_a_request_during_the_probe_is_not_closed() {
    giong::block_on(async {
        let connections = Rc::new(Cell::new(0));
        let base = scripted({
            let connections = connections.clone();
            move |_, h2| {
                connections.set(connections.get() + 1);
                // Answers requests at once and never acknowledges a PING.
                async move {
                    answer_requests(h2, |_| None).await;
                }
            }
        });
        let client = h2c();
        client.get(&base).send().await.unwrap();
        let probe = giong::spawn({
            let (client, url) = (client.clone(), base.clone());
            async move {
                client
                    .ping_idle_http2(&url, Duration::from_millis(500))
                    .await
            }
        });
        sleep(Duration::from_millis(20)).await;
        // Sent, answered and read well before the deadline.
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
        assert_eq!(client.connections(&base), (1, 1));
        let e = probe.await.unwrap().unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Timeout, "{e}");
        assert!(e.to_string().contains("1 left open"), "{e}");
        assert_eq!(client.connections(&base), (1, 1), "not closed");
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
        assert_eq!(connections.get(), 1);
    });
}

#[test]
fn a_goaway_read_with_the_acknowledgement_is_not_an_answer() {
    giong::block_on(async {
        let base = scripted(|n, h2| async move {
            answer_requests(h2, move |payload| {
                (n == 0).then(|| {
                    // GOAWAY and the acknowledgement in one write.
                    let mut goaway = 1u32.to_be_bytes().to_vec();
                    goaway.extend(0u32.to_be_bytes());
                    let mut out = frame(GOAWAY, 0, 0, &goaway);
                    out.extend(frame(PING, 1, 0, payload));
                    out
                })
            })
            .await;
        });
        let client = h2c();
        client.get(&base).send().await.unwrap();
        let e = client
            .ping_idle_http2(&base, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Closed, "{e}");
        assert!(e.to_string().contains("GOAWAY"), "{e}");
        eventually("the connection leaves the pool", || {
            client.connections(&base) == (0, 0)
        })
        .await;
        assert_eq!(
            client.get(&base).send().await.unwrap().text().unwrap(),
            "ok"
        );
    });
}
