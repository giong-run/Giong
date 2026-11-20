//! The HTTP/3 client: against `Server::serve_h3`, and against servers
//! scripted frame by frame that do what a real one rarely does.

use std::cell::RefCell;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use giong_quic::{ClientConfig, Connection, Endpoint, SendStream, ServerConfig};

use super::h3_hostile::{ask, data, headers, pattern, serve, Data, Seeded, Source};
use super::{Client, ClientBuilder, ErrorKind};
use crate::h3::frame::{self, kind, Header, Settings};
use crate::{Body, Handler, Request, Response, Server, Version};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut server = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key.into())
        .unwrap();
    server.alpn_protocols = vec![b"h3".to_vec()];
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h3".to_vec()];
    (server, client)
}

fn run<F: std::future::Future<Output = ()>>(test: impl FnOnce() -> F) {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(test())
        .unwrap();
}

/// A server endpoint on a port of its own, the address to reach it at, and
/// a client that speaks HTTP/3 to it.
fn pair() -> (Endpoint, String, Client) {
    pair_with(|builder| builder)
}

fn pair_with(settings: impl FnOnce(ClientBuilder) -> ClientBuilder) -> (Endpoint, String, Client) {
    let (server_tls, client_tls) = tls();
    let endpoint = Endpoint::server(
        "127.0.0.1:0".parse().unwrap(),
        ServerConfig::new(server_tls).unwrap(),
    )
    .unwrap();
    let addr: SocketAddr = endpoint.local_addr().unwrap();
    let builder = Client::builder()
        .http3_prior_knowledge(ClientConfig::new(client_tls).unwrap())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10));
    let client = settings(builder).build();
    (endpoint, format!("https://{addr}"), client)
}

thread_local! {
    /// The clients' addresses the application saw: one for each connection.
    static PEERS: RefCell<HashSet<SocketAddr>> = RefCell::new(HashSet::new());
}

struct App;

impl Handler for App {
    async fn call(&self, req: Request) -> Response {
        assert_eq!(req.version(), Version::Http3);
        PEERS.with(|p| p.borrow_mut().insert(req.peer_addr().unwrap()));
        match req.path() {
            "/" => Response::text("hello over HTTP/3")
                .header("x-method", req.method())
                .header(
                    "x-host",
                    String::from_utf8_lossy(req.header("host").unwrap()),
                ),
            "/echo" => Response::new(200).body(req.body().to_vec()),
            "/big" => Response::new(200).body(pattern(3 << 20)),
            "/empty" => Response::new(204),
            "/clear" => Response::text("cleared").header("alt-svc", "clear"),
            "/slow" => {
                giong::time::sleep(Duration::from_millis(30)).await;
                Response::text("slow")
            }
            "/events" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    for i in 0..1000 {
                        if tx.send(format!("event {i}\n")).await.is_err() {
                            return;
                        }
                        giong::time::sleep(Duration::from_millis(1)).await;
                    }
                }));
                Response::new(200).body(body)
            }
            _ => Response::new(404),
        }
    }
}

/// A giong-http server over HTTP/3, and a client for it.
fn served() -> (String, Client) {
    let (endpoint, url, client) = pair();
    drop(giong::spawn(async move {
        Server::new().serve_h3(endpoint, App).await
    }));
    (url, client)
}

#[test]
fn requests_and_responses_go_over_http3() {
    run(|| async {
        let (url, client) = served();
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.version(), Version::Http3);
        assert_eq!(res.text().unwrap(), "hello over HTTP/3");
        assert_eq!(res.header("x-method"), Some("GET"));
        assert_eq!(res.header("x-host"), Some(&url["https://".len()..]));

        // A body larger than the first flight, both ways.
        let body = pattern(700_000);
        let res = client
            .post(&format!("{url}/echo"))
            .body(body.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.body(), body);
        let res = client.get(&format!("{url}/big")).send().await.unwrap();
        assert_eq!(res.body(), pattern(3 << 20));

        // Responses without a body, and the status the application chose.
        let res = client.head(&format!("{url}/")).send().await.unwrap();
        assert_eq!((res.status(), res.body().len()), (200, 0));
        assert_eq!(res.header("content-length"), Some("17"));
        let res = client.get(&format!("{url}/empty")).send().await.unwrap();
        assert_eq!((res.status(), res.body().len()), (204, 0));
        let res = client.get(&format!("{url}/nowhere")).send().await.unwrap();
        assert_eq!(res.status(), 404);

        // All of it on one connection.
        assert_eq!(PEERS.with(|p| p.borrow().len()), 1);
    });
}

#[test]
fn a_burst_of_requests_shares_one_connection() {
    run(|| async {
        let (url, client) = served();
        let mut tasks = Vec::new();
        for i in 0..40 {
            let (client, url) = (client.clone(), url.clone());
            tasks.push(giong::spawn(async move {
                let path = if i % 2 == 0 { "/slow" } else { "/" };
                let res = client.get(&format!("{url}{path}")).send().await.unwrap();
                assert_eq!(res.status(), 200);
                res.into_body().len()
            }));
        }
        let mut total = 0;
        for task in tasks {
            total += task.await.unwrap();
        }
        assert_eq!(total, 20 * 4 + 20 * 17);
        assert_eq!(PEERS.with(|p| p.borrow().len()), 1);
    });
}

#[test]
fn a_response_is_read_as_it_arrives_and_can_be_given_up() {
    run(|| async {
        let (url, client) = served();
        let mut res = client
            .get(&format!("{url}/events"))
            .send_streaming()
            .await
            .unwrap();
        assert_eq!(res.version(), Version::Http3);
        let mut seen = Vec::new();
        while seen.len() < 60 {
            seen.extend(res.chunk().await.unwrap().expect("more events"));
        }
        assert!(seen.starts_with(b"event 0\nevent 1\n"));
        // Dropped in the middle: the stream alone is given up.
        drop(res);
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(PEERS.with(|p| p.borrow().len()), 1);
    });
}

/// What a scripted server sends first: its control stream with SETTINGS.
async fn control(conn: &Connection) -> SendStream {
    let mut control = conn.open_uni().await.unwrap();
    let mut out = vec![0x00];
    Settings::write_frame(&mut out, 1 << 20);
    control.write_all(&out).await.unwrap();
    control
}

/// The next request on `conn`, read to its end: the stream to answer on.
async fn request(conn: &Connection) -> SendStream {
    let (send, mut recv) = conn.accept_bi().await.unwrap();
    recv.read_to_end(1 << 20).await.unwrap();
    send
}

/// Answers every request on one connection with what `answer` gives for
/// the request's number, counted from 0; `None` resets the stream with
/// H3_REQUEST_REJECTED.
fn scripted(endpoint: Endpoint, answer: impl Fn(usize) -> Option<Vec<u8>> + 'static) {
    drop(giong::spawn(async move {
        let conn = endpoint.accept().await.unwrap();
        let _control = control(&conn).await;
        let mut n = 0;
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let _ = recv.read_to_end(1 << 20).await;
            match answer(n) {
                Some(bytes) => {
                    let _ = send.write_all(&bytes).await;
                    let _ = send.finish();
                }
                None => send.reset(0x10b),
            }
            n += 1;
        }
    }));
}

#[test]
fn interim_responses_trailers_and_unknown_frames_are_read_past() {
    run(|| async {
        let (endpoint, url, client) = pair();
        scripted(endpoint, |_| {
            let mut out = headers(Some(103), &[("link", "</style.css>; rel=preload")]);
            // A frame of a type nobody knows, between the others.
            Header::write(&mut out, 0x21, 3);
            out.extend_from_slice(b"abc");
            out.extend(headers(Some(200), &[("content-length", "9")]));
            out.extend(data(b"the "));
            out.extend(data(b""));
            out.extend(data(b"body!"));
            out.extend(headers(None, &[("x-checksum", "1")]));
            Some(out)
        });
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(res.body(), b"the body!");
        assert_eq!(res.header("x-checksum"), None);
    });
}

#[test]
fn a_request_the_server_rejected_unprocessed_is_sent_again() {
    run(|| async {
        let (endpoint, url, client) = pair();
        scripted(endpoint, |n| {
            (n >= 2).then(|| {
                let mut out = headers(Some(200), &[]);
                out.extend(data(n.to_string().as_bytes()));
                out
            })
        });
        // Whatever its method: nothing of it was processed.
        let res = client
            .post(&format!("{url}/"))
            .body("once")
            .send()
            .await
            .unwrap();
        assert_eq!(res.body(), b"2");
    });
}

#[test]
fn a_reset_stream_fails_its_request_alone() {
    run(|| async {
        let (endpoint, url, client) = pair();
        drop(giong::spawn(async move {
            let conn = endpoint.accept().await.unwrap();
            let _control = control(&conn).await;
            // H3_INTERNAL_ERROR, then an answer.
            request(&conn).await.reset(0x102);
            let mut send = request(&conn).await;
            send.write_all(&headers(Some(200), &[])).await.unwrap();
            send.finish().unwrap();
            let _ = conn.accept_bi().await;
        }));
        let error = client.get(&format!("{url}/")).send().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Closed, "{error}");
        assert!(error.to_string().contains("0x102"), "{error}");
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
    });
}

#[test]
fn a_malformed_response_fails_its_request_alone() {
    run(|| async {
        let (endpoint, url, client) = pair();
        scripted(endpoint, |n| {
            Some(match n {
                // An uppercase field name.
                0 => headers(Some(200), &[("X-Loud", "1")]),
                // No :status.
                1 => headers(None, &[("server", "x")]),
                // 101 does not exist in HTTP/3.
                2 => headers(Some(101), &[]),
                // More body than content-length says, and less.
                3 => {
                    let mut out = headers(Some(200), &[("content-length", "3")]);
                    out.extend(data(b"four"));
                    out
                }
                4 => {
                    let mut out = headers(Some(200), &[("content-length", "5")]);
                    out.extend(data(b"four"));
                    out
                }
                // Trailers with a pseudo-header field.
                5 => {
                    let mut out = headers(Some(200), &[]);
                    out.extend(headers(Some(200), &[]));
                    out
                }
                // A body where there cannot be one.
                6 => {
                    let mut out = headers(Some(204), &[]);
                    out.extend(data(b"x"));
                    out
                }
                _ => headers(Some(200), &[]),
            })
        });
        for n in 0..7 {
            let error = client.get(&format!("{url}/")).send().await.unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Protocol, "{n}: {error}");
        }
        // The connection outlived them all.
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
    });
}

#[test]
fn a_response_head_larger_than_allowed_is_refused() {
    run(|| async {
        let (endpoint, url, client) = pair_with(|b| b.max_head_size(4096));
        scripted(endpoint, |n| {
            let value = "v".repeat(if n == 0 { 9000 } else { 90 });
            Some(headers(Some(200), &[("x-large", &value)]))
        });
        let error = client.get(&format!("{url}/")).send().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TooLarge, "{error}");
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
    });
}

/// What the server learns when the client closes the connection on it.
type Told = Rc<RefCell<String>>;

/// Answers requests on `conn` with 200 until the connection ends, and
/// notes how it ended.
async fn answer_until_closed(conn: &Connection, told: &Told) {
    loop {
        match conn.accept_bi().await {
            Ok((mut send, mut recv)) => {
                let _ = recv.read_to_end(1 << 20).await;
                let _ = send.write_all(&headers(Some(200), &[])).await;
                let _ = send.finish();
            }
            Err(e) => {
                *told.borrow_mut() = e.to_string();
                return;
            }
        }
    }
}

/// A server whose answer to the first request breaks the protocol in a
/// way that ends the connection: the request fails, the server is told
/// why with `code`, and the next request gets a new connection.
fn broken_response(code: u32, answer: Vec<u8>) {
    run(|| async move {
        let (endpoint, url, client) = pair();
        let told = Told::default();
        let seen = told.clone();
        drop(giong::spawn(async move {
            let conn = endpoint.accept().await.unwrap();
            let _control = control(&conn).await;
            let mut send = request(&conn).await;
            let _ = send.write_all(&answer).await;
            let _ = send.finish();
            answer_until_closed(&conn, &told).await;
            let conn = endpoint.accept().await.unwrap();
            let _control = control(&conn).await;
            answer_until_closed(&conn, &Told::default()).await;
        }));
        let error = client.get(&format!("{url}/")).send().await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Protocol, "{code}: {error}");
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let seen = seen.borrow();
        assert!(seen.contains(&code.to_string()), "{code}: {seen}");
    });
}

#[test]
fn a_response_that_breaks_the_protocol_closes_the_connection_with_its_code() {
    // DATA before the head: H3_FRAME_UNEXPECTED (0x105 = 261).
    broken_response(261, data(b"early"));
    // A frame of HTTP/2 that HTTP/3 does not have (PING).
    let mut ping = Vec::new();
    Header::write(&mut ping, 0x06, 8);
    ping.extend_from_slice(&[0; 8]);
    broken_response(261, ping);
    // A control frame on a request stream.
    let mut settings = Vec::new();
    Settings::write_frame(&mut settings, 1 << 20);
    broken_response(261, settings);
    // A stream that ends inside a frame: H3_FRAME_ERROR (0x106 = 262).
    let mut cut = headers(Some(200), &[("server", "x")]);
    cut.truncate(cut.len() - 2);
    broken_response(262, cut);
    // A field section that needs a dynamic table:
    // QPACK_DECOMPRESSION_FAILED (0x200 = 512).
    let mut qpack = Vec::new();
    Header::write(&mut qpack, kind::HEADERS, 3);
    qpack.extend_from_slice(&[0x02, 0x00, 0x80]);
    broken_response(512, qpack);
    // PUSH_PROMISE, when no push was allowed: H3_ID_ERROR (0x108 = 264).
    let mut push = Vec::new();
    Header::write(&mut push, kind::PUSH_PROMISE, 3);
    push.extend_from_slice(&[0x00, 0x00, 0x00]);
    broken_response(264, push);
}

/// A server that breaks the protocol on a unidirectional stream it opens
/// with `bytes` (after its control stream when `with_control`): the
/// client closes the connection with `code` and goes on with a new one.
fn broken_stream(code: u32, with_control: bool, bytes: Vec<u8>) {
    run(|| async move {
        let (endpoint, url, client) = pair();
        let told = Told::default();
        let seen = told.clone();
        drop(giong::spawn(async move {
            let conn = endpoint.accept().await.unwrap();
            let _control = if with_control {
                Some(control(&conn).await)
            } else {
                None
            };
            let mut stream = conn.open_uni().await.unwrap();
            stream.write_all(&bytes).await.unwrap();
            answer_until_closed(&conn, &told).await;
            drop(stream);
            let conn = endpoint.accept().await.unwrap();
            let _control = control(&conn).await;
            answer_until_closed(&conn, &Told::default()).await;
        }));
        // The request that is under way when the connection closes may
        // fail; those after it get the new connection.
        for _ in 0..200 {
            let _ = client.get(&format!("{url}/")).send().await;
            if !seen.borrow().is_empty() {
                break;
            }
            giong::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            seen.borrow().contains(&code.to_string()),
            "{code}: {}",
            seen.borrow()
        );
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
    });
}

fn goaway(id: u64) -> Vec<u8> {
    let mut out = Vec::new();
    frame::write_goaway(&mut out, id);
    out
}

#[test]
fn a_server_stream_that_breaks_the_protocol_closes_the_connection_with_its_code() {
    // A control stream that does not start with SETTINGS:
    // H3_MISSING_SETTINGS (0x10a = 266).
    let mut out = vec![0x00];
    out.extend(goaway(0));
    broken_stream(266, false, out);
    // A second control stream: H3_STREAM_CREATION_ERROR (0x103 = 259).
    let mut out = vec![0x00];
    Settings::write_frame(&mut out, 1 << 20);
    broken_stream(259, true, out);
    // SETTINGS twice: H3_FRAME_UNEXPECTED (0x105 = 261).
    let mut out = vec![0x00];
    Settings::write_frame(&mut out, 1 << 20);
    Settings::write_frame(&mut out, 1 << 20);
    broken_stream(261, false, out);
    // A setting of HTTP/2: H3_SETTINGS_ERROR (0x109 = 265).
    let mut out = vec![0x00];
    Header::write(&mut out, kind::SETTINGS, 2);
    out.extend_from_slice(&[0x02, 0x00]);
    broken_stream(265, false, out);
    // MAX_PUSH_ID from a server: H3_FRAME_UNEXPECTED.
    let mut out = vec![0x00];
    Settings::write_frame(&mut out, 1 << 20);
    Header::write(&mut out, kind::MAX_PUSH_ID, 1);
    out.push(0x01);
    broken_stream(261, false, out);
    // GOAWAY going up, and one that names no request stream: H3_ID_ERROR
    // (0x108 = 264).
    let mut out = vec![0x00];
    Settings::write_frame(&mut out, 1 << 20);
    out.extend(goaway(400));
    out.extend(goaway(404));
    broken_stream(264, false, out);
    let mut out = vec![0x00];
    Settings::write_frame(&mut out, 1 << 20);
    out.extend(goaway(3));
    broken_stream(264, false, out);
    // A push stream, when no push was allowed: H3_ID_ERROR.
    broken_stream(264, true, vec![0x01, 0x00]);
    // The QPACK encoder stream filling a table that does not exist:
    // QPACK_ENCODER_STREAM_ERROR (0x201 = 513).
    broken_stream(513, true, vec![0x02, 0x3f, 0xe1, 0x1f]);
}

#[test]
fn what_a_server_may_do_on_its_other_streams_is_accepted() {
    run(|| async {
        let (endpoint, url, client) = pair();
        drop(giong::spawn(async move {
            let conn = endpoint.accept().await.unwrap();
            let mut control = control(&conn).await;
            // An unknown frame on the control stream.
            let mut out = Vec::new();
            Header::write(&mut out, 0x21, 3);
            out.extend_from_slice(b"abc");
            control.write_all(&out).await.unwrap();
            // The QPACK streams, saying what remains legal without a table.
            let mut encoder = conn.open_uni().await.unwrap();
            encoder.write_all(&[0x02, 0x20]).await.unwrap();
            let mut decoder = conn.open_uni().await.unwrap();
            decoder.write_all(&[0x03]).await.unwrap();
            // A stream of a type nobody knows.
            let mut unknown = conn.open_uni().await.unwrap();
            unknown.write_all(&[0x21, 1, 2, 3]).await.unwrap();
            answer_until_closed(&conn, &Told::default()).await;
        }));
        for _ in 0..5 {
            giong::time::sleep(Duration::from_millis(10)).await;
            let res = client.get(&format!("{url}/")).send().await.unwrap();
            assert_eq!(res.status(), 200);
        }
    });
}

#[test]
fn a_server_that_goes_away_gets_no_more_requests_on_that_connection() {
    run(|| async {
        let (endpoint, url, client) = pair();
        let connections = Rc::new(RefCell::new(0));
        let count = connections.clone();
        drop(giong::spawn(async move {
            loop {
                let conn = endpoint.accept().await.unwrap();
                *count.borrow_mut() += 1;
                let first = *count.borrow() == 1;
                drop(giong::spawn(async move {
                    let mut control = control(&conn).await;
                    if first {
                        // The first request is answered, then no more:
                        // GOAWAY names the next stream.
                        let mut send = request(&conn).await;
                        control.write_all(&goaway(4)).await.unwrap();
                        send.write_all(&headers(Some(200), &[])).await.unwrap();
                        send.finish().unwrap();
                    }
                    answer_until_closed(&conn, &Told::default()).await;
                }));
            }
        }));
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        giong::time::sleep(Duration::from_millis(50)).await;
        for _ in 0..3 {
            let res = client.get(&format!("{url}/")).send().await.unwrap();
            assert_eq!(res.status(), 200);
        }
        assert_eq!(*connections.borrow(), 2);
    });
}

#[test]
fn a_server_that_does_not_answer_is_a_connect_error_in_time() {
    run(|| async {
        // A UDP port nothing speaks QUIC on.
        let silent = giong::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = silent.local_addr().unwrap().port();
        let (_, client_tls) = tls();
        let client = Client::builder()
            .http3_prior_knowledge(ClientConfig::new(client_tls).unwrap())
            .connect_timeout(Duration::from_millis(300))
            .build();
        let started = std::time::Instant::now();
        let error = client
            .get(&format!("https://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(error.kind(), ErrorKind::Connect | ErrorKind::Timeout),
            "{error}"
        );
        assert!(error.to_string().contains("HTTP/3"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    });
}

#[test]
fn http_urls_and_other_clients_stay_on_tcp() {
    run(|| async {
        // Prior knowledge of HTTP/3 is about https: a plain URL is HTTP/1.1.
        let (_, client_tls) = tls();
        let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(giong::spawn(async move {
            Server::new().serve(listener, App1).await
        }));
        let client = Client::builder()
            .http3_prior_knowledge(ClientConfig::new(client_tls).unwrap())
            .build();
        let res = client.get(&format!("http://{addr}/")).send().await.unwrap();
        assert_eq!(res.version(), Version::Http11);
    });
}

struct App1;

impl Handler for App1 {
    async fn call(&self, _: Request) -> Response {
        Response::text("over TCP")
    }
}

#[test]
fn a_client_that_is_dropped_closes_its_connections() {
    run(|| async {
        let (endpoint, url, client) = pair();
        let told = Told::default();
        let seen = told.clone();
        drop(giong::spawn(async move {
            let conn = endpoint.accept().await.unwrap();
            let _control = control(&conn).await;
            answer_until_closed(&conn, &told).await;
        }));
        let res = client.get(&format!("{url}/")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        // A response still being read keeps its connection.
        let streaming = client
            .get(&format!("{url}/"))
            .send_streaming()
            .await
            .unwrap();
        drop(client);
        giong::time::sleep(Duration::from_millis(50)).await;
        assert!(seen.borrow().is_empty(), "{}", seen.borrow());
        drop(streaming);
        for _ in 0..200 {
            if !seen.borrow().is_empty() {
                break;
            }
            giong::time::sleep(Duration::from_millis(5)).await;
        }
        // H3_NO_ERROR (0x100 = 256).
        assert!(seen.borrow().contains("256"), "{}", seen.borrow());
    });
}

/// Finding HTTP/3 through `Alt-Svc`: the origin is an HTTPS server over
/// TCP that says where its HTTP/3 is.
#[cfg(feature = "tls")]
mod alt_svc {
    use super::*;
    use giong_tls::{Certificates, TlsAcceptor, TlsConnector};

    /// An HTTPS server over TCP whose responses carry `alt_svc(port)` as
    /// their `Alt-Svc`, with `Server::serve_h3` behind `port`; and a client
    /// that follows what the origin advertises.
    fn origin(alt_svc: impl Fn(u16) -> String + 'static, h3: bool) -> (String, Client) {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let (pem, key_pem) = (cert.cert.pem(), cert.key_pair.serialize_pem());
        let der = cert.cert.der().clone();
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
        let mut server_tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![der.clone()], key.into())
            .unwrap();
        server_tls.alpn_protocols = vec![b"h3".to_vec()];
        let mut roots = rustls::RootCertStore::empty();
        roots.add(der).unwrap();
        let mut client_tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"h3".to_vec()];

        // HTTP/3 on a UDP port, or a UDP port where nothing answers.
        let udp_port = if h3 {
            let endpoint = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(server_tls).unwrap(),
            )
            .unwrap();
            let port = endpoint.local_addr().unwrap().port();
            drop(giong::spawn(async move {
                Server::new().serve_h3(endpoint, App).await
            }));
            port
        } else {
            let silent = giong::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let port = silent.local_addr().unwrap().port();
            std::mem::forget(silent);
            port
        };
        let acceptor = TlsAcceptor::with_certificates(
            &Certificates::from_pem(pem.as_bytes(), key_pem.as_bytes()).unwrap(),
        )
        .unwrap();
        let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let value = alt_svc(udp_port);
        drop(giong::spawn(async move {
            let handler = move |_: Request| {
                let value = value.clone();
                async move { Response::text("over TCP").header("alt-svc", value) }
            };
            Server::new().serve_tls(listener, acceptor, handler).await
        }));
        let client = Client::builder()
            .tls(TlsConnector::with_root_pem(pem.as_bytes()).unwrap())
            .http3(ClientConfig::new(client_tls).unwrap())
            .connect_timeout(Duration::from_millis(400))
            .build();
        (format!("https://{addr}"), client)
    }

    #[test]
    fn an_origin_that_advertises_http3_gets_its_next_requests_over_it() {
        run(|| async {
            let (url, client) = origin(|port| format!("h3=\":{port}\"; ma=60"), true);
            // The first request has only TCP to go over.
            let res = client.get(&format!("{url}/")).send().await.unwrap();
            assert_eq!(res.version(), Version::Http2);
            assert_eq!(res.text().unwrap(), "over TCP");
            for _ in 0..3 {
                let res = client.get(&format!("{url}/")).send().await.unwrap();
                assert_eq!(res.version(), Version::Http3);
                assert_eq!(res.text().unwrap(), "hello over HTTP/3");
                // The origin is the one the URL names, whatever the port
                // HTTP/3 is on.
                assert_eq!(res.header("x-host"), Some(&url["https://".len()..]));
            }
            // The origin takes it back: TCP again.
            let res = client.get(&format!("{url}/clear")).send().await.unwrap();
            assert_eq!(res.version(), Version::Http3);
            let res = client.get(&format!("{url}/")).send().await.unwrap();
            assert_eq!(res.version(), Version::Http2);
        });
    }

    #[test]
    fn an_alternative_that_does_not_answer_costs_one_short_wait() {
        run(|| async {
            let (url, client) = origin(|port| format!("h3=\":{port}\""), false);
            let res = client.get(&format!("{url}/")).send().await.unwrap();
            assert_eq!(res.version(), Version::Http2);
            // The request that tries the alternative waits for it, then
            // goes over TCP.
            let started = std::time::Instant::now();
            let res = client.get(&format!("{url}/")).send().await.unwrap();
            assert_eq!(res.version(), Version::Http2);
            let waited = started.elapsed();
            assert!(
                waited >= Duration::from_millis(300) && waited < Duration::from_secs(3),
                "{waited:?}"
            );
            // Those after it do not try again, though the origin goes on
            // advertising.
            let started = std::time::Instant::now();
            for _ in 0..5 {
                let res = client.get(&format!("{url}/")).send().await.unwrap();
                assert_eq!(res.version(), Version::Http2);
            }
            assert!(started.elapsed() < Duration::from_millis(300));
        });
    }

    #[test]
    fn an_alternative_on_another_host_is_not_followed() {
        run(|| async {
            let (url, client) = origin(|port| format!("h3=\"127.0.0.1:{port}\""), true);
            for _ in 0..3 {
                let res = client.get(&format!("{url}/")).send().await.unwrap();
                assert_eq!(res.version(), Version::Http2);
            }
        });
    }

    #[test]
    fn prior_knowledge_does_not_wait_for_an_advertisement() {
        run(|| async {
            let (endpoint, url, client) = pair();
            drop(giong::spawn(async move {
                Server::new().serve_h3(endpoint, App).await
            }));
            let res = client.get(&format!("{url}/")).send().await.unwrap();
            assert_eq!(res.version(), Version::Http3);
        });
    }
}

/// Requests to servers that do what `source` says, a new server and a
/// new client for each seed; gives how many were answered and how many
/// failed.
async fn against_hostile_servers<S: Source>(source: impl Fn(u64) -> S) -> (usize, usize) {
    let (mut answered, mut failed) = (0, 0);
    for seed in 1..=120u64 {
        let (endpoint, url, client) = pair();
        let mut src = source(seed);
        let mut fork = src.fork();
        let server = giong::spawn(async move {
            while let Some(conn) = endpoint.accept().await {
                drop(giong::spawn(serve(conn, fork.fork())));
            }
        });
        for _ in 0..4 {
            // Some one after another, some at once.
            let mut tasks = Vec::new();
            for _ in 0..1 + src.below(6) {
                let (client, url) = (client.clone(), format!("{url}/"));
                tasks.push(giong::spawn(async move { ask(&client, &url).await }));
            }
            for task in tasks {
                match task.await {
                    Ok(true) => answered += 1,
                    Ok(false) => failed += 1,
                    Err(_) => panic!("seed {seed}: a request panicked"),
                }
            }
        }
        drop(client);
        server.abort();
    }
    (answered, failed)
}

#[test]
fn a_hostile_server_neither_hangs_nor_fools_the_client() {
    run(|| async {
        let (answered, failed) =
            against_hostile_servers(|seed| Seeded(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15))).await;
        // Both happen, or the test checks nothing.
        assert!(answered > 300, "{answered} answered, {failed} failed");
        assert!(failed > 50, "{answered} answered, {failed} failed");
    });
}

/// The same from bytes, as the `h3_client` fuzz target reads them.
#[test]
fn a_hostile_server_made_of_fuzz_data_neither_hangs_nor_fools_the_client() {
    run(|| async {
        let (answered, failed) = against_hostile_servers(|seed| {
            let mut rng = Seeded(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            Data::new(&rng.bytes(400))
        })
        .await;
        assert!(answered > 300, "{answered} answered, {failed} failed");
        assert!(failed > 50, "{answered} answered, {failed} failed");
    });
}
