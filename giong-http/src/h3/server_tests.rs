//! The HTTP/3 server end to end: a giong-quic client that writes the
//! frames by hand (so that it can also write wrong ones) against
//! `Server::serve_h3` on the same worker.

use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use giong_quic::{ClientConfig, Connection, Endpoint, SendStream, ServerConfig};

use super::frame::{kind, read_varint, write_varint, Header, Settings};
use super::qpack::{self, Encoder};
use crate::h2::hpack::Index;
use crate::{Body, Handler, Request, Response, Server, Version};

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
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

/// The application under test.
struct App;

impl Handler for App {
    async fn call(&self, req: Request) -> Response {
        match req.path() {
            "/" => {
                assert_eq!(req.version(), Version::Http3);
                assert!(req.peer_addr().is_some());
                Response::text("hello over HTTP/3")
                    .header("x-version", req.version().to_string())
                    .header(
                        "x-host",
                        String::from_utf8_lossy(req.header("host").unwrap()),
                    )
            }
            "/early" => Response::text(req.is_early_data().to_string()),
            "/echo" => Response::new(200).body(req.body().to_vec()),
            "/big" => Response::new(200).body(pattern(1 << 20)),
            "/empty" => Response::new(204),
            "/upload" => {
                let body = req.body_stream().expect("a streamed body");
                let mut sum = 0u64;
                let mut len = 0u64;
                loop {
                    match body.chunk().await {
                        Ok(Some(piece)) => {
                            len += piece.len() as u64;
                            sum += piece.iter().map(|&b| u64::from(b)).sum::<u64>();
                        }
                        Ok(None) => break Response::text(format!("{len} {sum}")),
                        Err(e) => {
                            break Response::new(400).body(format!(
                                "{:?}",
                                giong::error::code(&e).map(|c| c.to_string())
                            ))
                        }
                    }
                }
            }
            "/events" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    for piece in ["one ", "two ", "three"] {
                        if tx.send(piece).await.is_err() {
                            return;
                        }
                    }
                    let _ = tx.send(pattern(100_000)).await;
                }));
                Response::new(200).body(body)
            }
            "/file" => {
                let path = std::env::temp_dir().join(format!("giong-h3-{}", std::process::id()));
                std::fs::write(&path, pattern(300_000)).unwrap();
                let file = giong::fs::File::open(&path).await.unwrap();
                let _ = std::fs::remove_file(&path);
                Response::new(200).body(Body::file(file).unwrap())
            }
            "/sleep" => {
                giong::time::sleep(std::time::Duration::from_millis(200)).await;
                Response::text("slept")
            }
            "/panic" => panic!("the handler panics on purpose"),
            _ => Response::new(404),
        }
    }

    fn stream_body(&self, req: &Request) -> Option<u64> {
        (req.path() == "/upload").then_some(1 << 20)
    }
}

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + i / 253) as u8).collect()
}

/// A client connection with its control stream open.
struct Client {
    conn: Rc<Connection>,
    control: SendStream,
    _endpoint: Endpoint,
}

/// Starts a server with `server` and connects a client to it, without the
/// client's control stream.
async fn connect_bare(server: Server) -> (Rc<Connection>, Endpoint) {
    let (server_tls, client_tls) = tls();
    let endpoint = Endpoint::server(
        "127.0.0.1:0".parse().unwrap(),
        ServerConfig::new(server_tls).unwrap(),
    )
    .unwrap();
    let addr: SocketAddr = endpoint.local_addr().unwrap();
    drop(giong::spawn(
        async move { server.serve_h3(endpoint, App).await },
    ));
    let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let cfg = ClientConfig::new(client_tls).unwrap();
    let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
    (Rc::new(conn), client)
}

async fn connect_to(server: Server) -> Client {
    let (conn, endpoint) = connect_bare(server).await;
    let mut control = conn.open_uni().await.unwrap();
    let mut out = vec![0x00];
    Settings::write_frame(&mut out, 1 << 20);
    control.write_all(&out).await.unwrap();
    Client {
        conn,
        control,
        _endpoint: endpoint,
    }
}

async fn connect() -> Client {
    connect_to(Server::new()).await
}

/// A HEADERS frame with these fields.
fn headers(fields: &[(&str, &str)]) -> Vec<u8> {
    let mut block = Vec::new();
    Encoder::begin(&mut block);
    for (name, value) in fields {
        Encoder::field(name.as_bytes(), value.as_bytes(), Index::Yes, &mut block);
    }
    let mut out = Vec::new();
    Header::write(&mut out, kind::HEADERS, block.len() as u64);
    out.extend_from_slice(&block);
    out
}

fn get(path: &str) -> Vec<u8> {
    headers(&[
        (":method", "GET"),
        (":scheme", "https"),
        (":authority", "localhost"),
        (":path", path),
    ])
}

fn post(path: &str, extra: &[(&str, &str)]) -> Vec<u8> {
    let mut fields = vec![
        (":method", "POST"),
        (":scheme", "https"),
        (":authority", "localhost"),
        (":path", path),
    ];
    fields.extend_from_slice(extra);
    headers(&fields)
}

fn data(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    Header::write(&mut out, kind::DATA, body.len() as u64);
    out.extend_from_slice(body);
    out
}

#[derive(Debug, Default)]
struct Reply {
    /// The statuses of the interim responses, then of the final one.
    statuses: Vec<u16>,
    fields: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn status(&self) -> u16 {
        *self.statuses.last().expect("a response")
    }

    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Reads the frames of a whole response stream.
fn parse(mut bytes: &[u8]) -> Reply {
    let mut reply = Reply::default();
    while !bytes.is_empty() {
        let (kind, a) = read_varint(bytes).expect("a frame type");
        let (len, b) = read_varint(&bytes[a..]).expect("a frame length");
        let payload = &bytes[a + b..a + b + len as usize];
        bytes = &bytes[a + b + len as usize..];
        match kind {
            kind::HEADERS => {
                reply.fields.clear();
                qpack::decode(payload, |name, value| {
                    let (name, value) = (
                        String::from_utf8(name.to_vec()).unwrap(),
                        String::from_utf8(value.to_vec()).unwrap(),
                    );
                    if name == ":status" {
                        reply.statuses.push(value.parse().unwrap());
                    } else {
                        reply.fields.push((name, value));
                    }
                })
                .expect("a field section that decodes");
            }
            kind::DATA => reply.body.extend_from_slice(payload),
            other => panic!("unexpected frame type {other}"),
        }
    }
    reply
}

/// Sends `request` on a new stream and reads the whole response.
async fn roundtrip(client: &Client, request: Vec<u8>) -> io::Result<Reply> {
    let (mut send, mut recv) = client.conn.open_bi().await?;
    send.write_all(&request).await?;
    send.finish()?;
    Ok(parse(&recv.read_to_end(8 << 20).await?))
}

fn run<F: std::future::Future<Output = ()>>(test: impl FnOnce() -> F) {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(test())
        .unwrap();
}

fn text(e: &io::Error) -> String {
    e.to_string()
}

#[test]
fn requests_without_a_body_are_answered() {
    run(|| async {
        let client = connect().await;
        let reply = roundtrip(&client, get("/")).await.unwrap();
        assert_eq!(reply.statuses, [200]);
        assert_eq!(reply.body, b"hello over HTTP/3");
        assert_eq!(reply.field("x-version"), Some("HTTP/3"));
        assert_eq!(reply.field("x-host"), Some("localhost"));
        assert_eq!(reply.field("content-length"), Some("17"));
        assert!(reply.field("date").is_some());

        // HEAD: the fields of GET, no body.
        let mut head = headers(&[
            (":method", "HEAD"),
            (":scheme", "https"),
            (":authority", "localhost"),
            (":path", "/"),
        ]);
        // Frames of unknown types are ignored, wherever they are.
        Header::write(&mut head, 0x21 + 0x1f * 3, 4);
        head.extend_from_slice(b"skip");
        let reply = roundtrip(&client, head).await.unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.field("content-length"), Some("17"));
        assert!(reply.body.is_empty());

        let reply = roundtrip(&client, get("/empty")).await.unwrap();
        assert_eq!(reply.status(), 204);
        assert_eq!(reply.field("content-length"), None);
        assert_eq!(
            roundtrip(&client, get("/nowhere")).await.unwrap().status(),
            404
        );

        // Many at once on one connection, with a large response among them.
        let client = Rc::new(client);
        let mut tasks = Vec::new();
        for i in 0..40 {
            let client = client.clone();
            tasks.push(giong::spawn(async move {
                if i % 8 == 0 {
                    let reply = roundtrip(&client, get("/big")).await.unwrap();
                    assert_eq!(reply.body, pattern(1 << 20));
                } else {
                    let reply = roundtrip(&client, get("/")).await.unwrap();
                    assert_eq!(reply.body, b"hello over HTTP/3");
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    });
}

#[test]
fn bodies_are_received_whole_or_as_a_stream() {
    run(|| async {
        let client = connect().await;
        // Buffered, in several DATA frames, with trailers after them.
        let body = pattern(200_000);
        let mut request = post("/echo", &[("content-length", "200000")]);
        request.extend(data(&body[..1]));
        request.extend(data(&[]));
        request.extend(data(&body[1..70_000]));
        request.extend(data(&body[70_000..]));
        request.extend(headers(&[("x-checksum", "abc")]));
        let reply = roundtrip(&client, request).await.unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.body, body);

        // Without a length.
        let mut request = post("/echo", &[]);
        request.extend(data(b"no length"));
        let reply = roundtrip(&client, request).await.unwrap();
        assert_eq!(reply.body, b"no length");

        // Streamed to the handler, which is called before the body is in,
        // and asked to go on with `100 Continue` when it reads.
        let body = pattern(500_000);
        let sum: u64 = body.iter().map(|&b| u64::from(b)).sum();
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        send.write_all(&post("/upload", &[("expect", "100-continue")]))
            .await
            .unwrap();
        for piece in body.chunks(50_000) {
            send.write_all(&data(piece)).await.unwrap();
        }
        send.finish().unwrap();
        let reply = parse(&recv.read_to_end(1 << 20).await.unwrap());
        assert_eq!(reply.statuses, [100, 200]);
        assert_eq!(reply.body, format!("500000 {sum}").into_bytes());

        // Past the limit the handler gave: it is told, and answers.
        let mut request = post("/upload", &[]);
        request.extend(data(&pattern((1 << 20) + 1)));
        let reply = roundtrip(&client, request).await.unwrap();
        assert_eq!(reply.status(), 400);
        assert_eq!(reply.body, b"Some(\"GIONG-E1111\")");
        // Announced as longer than the limit: refused before the handler.
        let request = post("/upload", &[("content-length", "1048577")]);
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        send.write_all(&request).await.unwrap();
        let reply = parse(&recv.read_to_end(1 << 20).await.unwrap());
        assert_eq!(reply.status(), 413);
        drop(send);
    });
}

#[test]
fn responses_are_streamed_and_read_from_files() {
    run(|| async {
        let client = connect().await;
        let reply = roundtrip(&client, get("/events")).await.unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.field("content-length"), None);
        let mut want = b"one two three".to_vec();
        want.extend(pattern(100_000));
        assert_eq!(reply.body, want);

        let reply = roundtrip(&client, get("/file")).await.unwrap();
        assert_eq!(reply.status(), 200);
        assert_eq!(reply.field("content-length"), Some("300000"));
        assert_eq!(reply.body, pattern(300_000));
    });
}

#[test]
fn the_limits_of_the_server_apply() {
    run(|| async {
        let server = Server::new().max_head_size(2048).max_body_size(10_000);
        let client = connect_to(server).await;
        // A field list over the limit.
        let long = "v".repeat(3000);
        let mut fields = vec![
            (":method", "GET"),
            (":scheme", "https"),
            (":authority", "localhost"),
            (":path", "/"),
        ];
        fields.push(("x-long", &long));
        let reply = roundtrip(&client, headers(&fields)).await.unwrap();
        assert_eq!(reply.status(), 431);
        // A body announced, or found, to be over the limit.
        let mut request = post("/echo", &[("content-length", "10001")]);
        request.extend(data(&pattern(10_001)));
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        send.write_all(&request).await.unwrap();
        assert_eq!(
            parse(&recv.read_to_end(1 << 20).await.unwrap()).status(),
            413
        );
        drop(send);
        let mut request = post("/echo", &[]);
        request.extend(data(&pattern(10_001)));
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        send.write_all(&request).await.unwrap();
        assert_eq!(
            parse(&recv.read_to_end(1 << 20).await.unwrap()).status(),
            413
        );
        drop(send);
        // At the limit it passes.
        let mut request = post("/echo", &[]);
        request.extend(data(&pattern(10_000)));
        let reply = roundtrip(&client, request).await.unwrap();
        assert_eq!(reply.body, pattern(10_000));
    });
}

#[test]
fn a_bad_request_loses_its_stream_and_the_connection_goes_on() {
    run(|| async {
        let client = connect().await;
        // Malformed requests (RFC 9114, section 4.1.2): H3_MESSAGE_ERROR,
        // 0x10e = 270.
        let malformed: Vec<Vec<u8>> = vec![
            // No :path.
            headers(&[
                (":method", "GET"),
                (":scheme", "https"),
                (":authority", "x"),
            ]),
            // A connection-specific field.
            {
                let mut fields = get("/");
                fields.clear();
                fields.extend(headers(&[
                    (":method", "GET"),
                    (":scheme", "https"),
                    (":authority", "localhost"),
                    (":path", "/"),
                    ("connection", "close"),
                ]));
                fields
            },
            // Neither :authority nor host (section 4.3.1).
            headers(&[(":method", "GET"), (":scheme", "https"), (":path", "/")]),
            // A pseudo-header field after a regular one.
            headers(&[
                (":method", "GET"),
                (":scheme", "https"),
                ("accept", "*/*"),
                (":path", "/"),
            ]),
            // A body longer than its content-length.
            {
                let mut request = post("/echo", &[("content-length", "3")]);
                request.extend(data(b"four"));
                request
            },
            // And shorter.
            {
                let mut request = post("/echo", &[("content-length", "5")]);
                request.extend(data(b"four"));
                request
            },
            // Trailers with a pseudo-header field.
            {
                let mut request = post("/echo", &[]);
                request.extend(data(b"body"));
                request.extend(headers(&[(":status", "200")]));
                request
            },
        ];
        for (i, request) in malformed.into_iter().enumerate() {
            let error = roundtrip(&client, request).await.expect_err("a reset");
            assert_eq!(
                giong::error::code(&error),
                Some(giong::error::Code::QuicStreamReset),
                "request {i}: {error}"
            );
            assert!(text(&error).contains("code 270"), "request {i}: {error}");
        }
        // A stream that ends before its request does: H3_REQUEST_INCOMPLETE.
        let error = roundtrip(&client, Vec::new()).await.expect_err("a reset");
        assert!(text(&error).contains("code 269"), "{error}");
        // A request the client gives up: its response does not end as if
        // it were whole. H3_REQUEST_CANCELLED, 0x10c = 268.
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        send.write_all(&post("/echo", &[])).await.unwrap();
        send.reset(0x10c);
        let error = recv.read_to_end(1 << 20).await.expect_err("a reset");
        assert!(text(&error).contains("code 268"), "{error}");
        // A handler that panics: 500, and nothing else is lost.
        assert_eq!(
            roundtrip(&client, get("/panic")).await.unwrap().status(),
            500
        );
        assert_eq!(roundtrip(&client, get("/")).await.unwrap().status(), 200);
    });
}

#[test]
fn a_request_dropped_mid_frame_is_cancelled_and_the_connection_lives_on() {
    run(|| async {
        let client = connect().await;
        // An upload whose sending half is dropped inside a DATA frame, as
        // when its future is cancelled: the stream is reset, not ended
        // with a FIN the server would read as a frame cut short (a
        // connection error).
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        let mut request = post("/echo", &[("content-length", "100000")]);
        let mut frame = Vec::new();
        Header::write(&mut frame, kind::DATA, 100_000);
        request.extend(frame);
        request.extend(pattern(5000));
        send.write_all(&request).await.unwrap();
        drop(send);
        assert!(recv.read_to_end(1 << 20).await.is_err());
        // Other requests on the connection go on.
        for _ in 0..3 {
            assert_eq!(roundtrip(&client, get("/")).await.unwrap().status(), 200);
        }
    });
}

#[test]
fn a_header_section_announced_too_large_gets_its_431_at_once() {
    run(|| async {
        let client = connect().await;
        // A HEADERS frame that says it is a gigabyte, with little of it
        // sent: the answer does not wait for the rest.
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        let mut out = Vec::new();
        Header::write(&mut out, kind::HEADERS, 1 << 30);
        out.extend_from_slice(&[0; 100]);
        send.write_all(&out).await.unwrap();
        let reply =
            giong::time::timeout(std::time::Duration::from_secs(3), recv.read_to_end(1 << 20))
                .await
                .expect("the 431 waited for the rest of the frame")
                .unwrap();
        assert_eq!(parse(&reply).status(), 431);
        drop(send);
        assert_eq!(roundtrip(&client, get("/")).await.unwrap().status(), 200);
    });
}

/// `n` frames of a reserved (grease) type, which the server ignores.
fn unknown_frames(n: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for _ in 0..n {
        Header::write(&mut out, 0x21, 1);
        out.push(0);
    }
    out
}

#[test]
fn unknown_control_frames_are_bounded_per_second_not_for_the_connection_s_life() {
    run(|| async {
        // A long session that sends extension frames now and then keeps
        // its connection, past any count over its whole life.
        let mut client = connect().await;
        for _ in 0..3 {
            client
                .control
                .write_all(&unknown_frames(3000))
                .await
                .unwrap();
            giong::time::sleep(std::time::Duration::from_millis(1100)).await;
            assert_eq!(roundtrip(&client, get("/")).await.unwrap().status(), 200);
        }
        // A flood within a second ends it: H3_EXCESSIVE_LOAD, 0x107 = 263.
        let mut client = connect().await;
        client
            .control
            .write_all(&unknown_frames(5000))
            .await
            .unwrap();
        let why = closed_with(&client).await;
        assert!(why.contains("263"), "{why}");
    });
}

/// What a client sees of a connection the server closed with an error.
async fn closed_with(client: &Client) -> String {
    // The next thing done on the connection reports its end.
    loop {
        match roundtrip(client, get("/")).await {
            Ok(_) => giong::time::sleep(std::time::Duration::from_millis(5)).await,
            Err(e) => return text(&e),
        }
    }
}

#[test]
fn breaking_the_protocol_closes_the_connection_with_its_code() {
    run(|| async {
        // DATA before HEADERS: H3_FRAME_UNEXPECTED (0x105).
        let client = connect().await;
        let _ = roundtrip(&client, data(b"early")).await;
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x105") || seen.contains("261"), "{seen}");

        // A frame of HTTP/2 that HTTP/3 does not have (PING, 0x06).
        let client = connect().await;
        let mut request = Vec::new();
        Header::write(&mut request, 0x06, 8);
        request.extend_from_slice(&[0; 8]);
        let _ = roundtrip(&client, request).await;
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x105") || seen.contains("261"), "{seen}");

        // A stream that ends inside a frame: H3_FRAME_ERROR (0x106).
        let client = connect().await;
        let mut request = get("/");
        request.truncate(request.len() - 3);
        let _ = roundtrip(&client, request).await;
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x106") || seen.contains("262"), "{seen}");

        // A field section that does not decode:
        // QPACK_DECOMPRESSION_FAILED (0x200).
        let client = connect().await;
        let mut request = Vec::new();
        Header::write(&mut request, kind::HEADERS, 3);
        // Required Insert Count 1: a dynamic table that does not exist.
        request.extend_from_slice(&[0x02, 0x00, 0x80]);
        let _ = roundtrip(&client, request).await;
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x200") || seen.contains("512"), "{seen}");

        // A second control stream: H3_STREAM_CREATION_ERROR (0x103).
        let client = connect().await;
        let mut second = client.conn.open_uni().await.unwrap();
        let mut out = vec![0x00];
        Settings::write_frame(&mut out, 1 << 20);
        second.write_all(&out).await.unwrap();
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x103") || seen.contains("259"), "{seen}");

        // A control stream that does not start with SETTINGS:
        // H3_MISSING_SETTINGS (0x10a).
        let (conn, endpoint) = connect_bare(Server::new()).await;
        let mut control = conn.open_uni().await.unwrap();
        let mut out = vec![0x00];
        super::frame::write_goaway(&mut out, 0);
        control.write_all(&out).await.unwrap();
        let client = Client {
            conn,
            control,
            _endpoint: endpoint,
        };
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x10a") || seen.contains("266"), "{seen}");

        // The control stream closed: H3_CLOSED_CRITICAL_STREAM (0x104).
        let mut client = connect().await;
        client.control.finish().unwrap();
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x104") || seen.contains("260"), "{seen}");

        // SETTINGS twice: H3_FRAME_UNEXPECTED.
        let mut client = connect().await;
        let mut out = Vec::new();
        Settings::write_frame(&mut out, 1 << 20);
        client.control.write_all(&out).await.unwrap();
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x105") || seen.contains("261"), "{seen}");

        // A setting sent twice, one nobody defined: H3_SETTINGS_ERROR
        // (0x109).
        let (conn, endpoint) = connect_bare(Server::new()).await;
        let mut control = conn.open_uni().await.unwrap();
        let mut out = vec![0x00];
        Header::write(&mut out, kind::SETTINGS, 6);
        out.extend_from_slice(&[0x40, 0x40, 0x00, 0x40, 0x40, 0x00]);
        control.write_all(&out).await.unwrap();
        let client = Client {
            conn,
            control,
            _endpoint: endpoint,
        };
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x109") || seen.contains("265"), "{seen}");

        // GOAWAY going up: H3_ID_ERROR (0x108).
        let mut client = connect().await;
        let mut out = Vec::new();
        super::frame::write_goaway(&mut out, 0);
        super::frame::write_goaway(&mut out, 4);
        client.control.write_all(&out).await.unwrap();
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x108") || seen.contains("264"), "{seen}");

        // A GOAWAY that says it is a gibibyte long, and none of it sent:
        // H3_EXCESSIVE_LOAD (0x107), without waiting for the payload.
        let mut client = connect().await;
        let out = [0x07, 0xc0, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00];
        client.control.write_all(&out).await.unwrap();
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x107") || seen.contains("263"), "{seen}");

        // One that is only too long for its integer: H3_FRAME_ERROR.
        let mut client = connect().await;
        let mut out = Vec::new();
        Header::write(&mut out, kind::GOAWAY, 9);
        out.extend_from_slice(&[0; 9]);
        client.control.write_all(&out).await.unwrap();
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x106") || seen.contains("262"), "{seen}");

        // A push stream from a client: H3_STREAM_CREATION_ERROR.
        let client = connect().await;
        let mut push = client.conn.open_uni().await.unwrap();
        push.write_all(&[0x01, 0x00]).await.unwrap();
        let seen = closed_with(&client).await;
        assert!(seen.contains("0x103") || seen.contains("259"), "{seen}");
    });
}

#[test]
fn what_a_client_may_do_on_its_other_streams_is_accepted() {
    run(|| async {
        let mut client = connect().await;
        // Unknown frames on the control stream, MAX_PUSH_ID going up, and
        // a CANCEL_PUSH for a push that was allowed.
        let mut out = Vec::new();
        Header::write(&mut out, 0x21, 3);
        out.extend_from_slice(b"abc");
        for (kind, id) in [
            (kind::MAX_PUSH_ID, 3),
            (kind::MAX_PUSH_ID, 7),
            (kind::CANCEL_PUSH, 5),
            // GOAWAY more than once, as long as it does not go up.
            (kind::GOAWAY, 4),
            (kind::GOAWAY, 4),
            (kind::GOAWAY, 0),
        ] {
            let mut payload = Vec::new();
            write_varint(&mut payload, id);
            Header::write(&mut out, kind, payload.len() as u64);
            out.extend_from_slice(&payload);
        }
        client.control.write_all(&out).await.unwrap();
        // The QPACK streams, saying what remains legal without a table.
        let mut encoder = client.conn.open_uni().await.unwrap();
        encoder.write_all(&[0x02, 0x20]).await.unwrap();
        let mut decoder = client.conn.open_uni().await.unwrap();
        decoder.write_all(&[0x03, 0x44]).await.unwrap();
        // A stream of a type nobody knows: not an error.
        let mut unknown = client.conn.open_uni().await.unwrap();
        unknown.write_all(&[0x21, 1, 2, 3]).await.unwrap();
        for _ in 0..5 {
            giong::time::sleep(std::time::Duration::from_millis(10)).await;
            assert_eq!(roundtrip(&client, get("/")).await.unwrap().status(), 200);
        }
    });
}

/// Reads the server's control stream up to and including a GOAWAY frame
/// and gives the stream ID it announces.
async fn goaway(conn: &Connection) -> u64 {
    let mut control = conn.accept_uni().await.unwrap();
    let mut bytes = Vec::new();
    loop {
        // The stream type, then frames.
        let mut rest = bytes.get(1..).unwrap_or(&[]);
        while let Some((kind, a)) = read_varint(rest) {
            let Some((len, b)) = read_varint(&rest[a..]) else {
                break;
            };
            let Some(payload) = rest.get(a + b..a + b + len as usize) else {
                break;
            };
            if kind == kind::GOAWAY {
                assert_eq!(bytes[0], 0x00, "the control stream");
                return read_varint(payload).expect("a stream ID").0;
            }
            rest = &rest[a + b + len as usize..];
        }
        let piece = control.read().await.unwrap().expect("a GOAWAY");
        bytes.extend_from_slice(&piece);
    }
}

#[test]
fn shutdown_finishes_accepted_requests_and_rejects_later_ones() {
    run(|| async {
        let client = connect().await;
        assert_eq!(roundtrip(&client, get("/")).await.unwrap().status(), 200);
        // A request in progress when the shutdown is asked for.
        let (mut send, mut recv) = client.conn.open_bi().await.unwrap();
        send.write_all(&get("/sleep")).await.unwrap();
        send.finish().unwrap();
        giong::time::sleep(std::time::Duration::from_millis(50)).await;
        giong::handle().shutdown();

        // GOAWAY names the first stream that will not be handled (RFC
        // 9114, section 5.2): streams 0 and 4 were accepted.
        assert_eq!(goaway(&client.conn).await, 8);
        // A request after it is rejected so that it can be retried on
        // another connection: H3_REQUEST_REJECTED, 0x10b = 267.
        let error = roundtrip(&client, get("/")).await.expect_err("a reset");
        assert!(text(&error).contains("code 267"), "{error}");
        // The one in progress is finished ...
        let reply = parse(&recv.read_to_end(1 << 20).await.unwrap());
        assert_eq!((reply.status(), &reply.body[..]), (200, &b"slept"[..]));
        // ... and then the connection is closed without an error:
        // H3_NO_ERROR, 0x100 = 256.
        let end = closed_with(&client).await;
        assert!(end.contains("closed by peer: 256"), "{end}");
    });
}

/// Plays `input` on a new connection to the server at `addr` and checks
/// every response that arrives whole. Returns how many did.
async fn play(
    endpoint: &Endpoint,
    addr: SocketAddr,
    cfg: &ClientConfig,
    input: &crate::invariants::H3Input,
) -> usize {
    let conn = endpoint.connect(addr, "localhost", cfg).await.unwrap();
    // Streams stay open until the responses are read: an error on any of
    // them only shows in what the server does next.
    let mut open = Vec::new();
    for bytes in input.control.iter().chain(&input.uni) {
        let Ok(mut stream) = conn.open_uni().await else {
            break;
        };
        let _ = stream.write_all(bytes).await;
        open.push(stream);
    }
    let mut responses = Vec::new();
    for request in &input.requests {
        let Ok((mut send, recv)) = conn.open_bi().await else {
            break;
        };
        let _ = send.write_all(&request.bytes).await;
        if request.finish {
            let _ = send.finish();
        } else {
            send.reset(0x10c);
        }
        responses.push(recv);
    }
    let mut whole = 0;
    for mut recv in responses {
        if let Ok(output) = recv.read_to_end(1 << 20).await {
            crate::invariants::check_h3_response(&output);
            whole += 1;
        }
    }
    conn.close(0x100, b"");
    whole
}

#[test]
fn connections_answer_with_well_formed_responses_whatever_the_client_sends() {
    use crate::invariants::h3_connection::{server, Fuzzed};
    use crate::invariants::h3_input;

    const ROUNDS: usize = 400;
    let mut state: u64 = std::env::var("GIONG_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xE703_7ED1_A0B4_28DB);
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    run(|| async move {
        let (server_tls, client_tls) = tls();
        let endpoint = Endpoint::server(
            "127.0.0.1:0".parse().unwrap(),
            ServerConfig::new(server_tls).unwrap(),
        )
        .unwrap();
        let addr: SocketAddr = endpoint.local_addr().unwrap();
        drop(giong::spawn(async move {
            server().serve_h3(endpoint, Fuzzed).await
        }));
        let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let cfg = ClientConfig::new(client_tls).unwrap();
        let mut whole = 0;
        for round in 0..ROUNDS {
            let len = 1 + next() as usize % 120;
            let data: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let input = h3_input(&data);
            let played = giong::time::timeout(
                std::time::Duration::from_secs(10),
                play(&client, addr, &cfg, &input),
            )
            .await;
            whole += played.unwrap_or_else(|_| panic!("round {round} hangs: {data:?}"));
        }
        assert!(whole > ROUNDS / 4, "only {whole} responses");
    });
}

/// A server that takes 0-RTT data: requests that come in it are marked,
/// and those of methods that are not safe are answered 425 Too Early
/// without reaching the handler.
#[test]
fn requests_in_early_data_are_marked_and_unsafe_ones_wait() {
    run(|| async {
        let (server_tls, client_tls) = tls();
        let endpoint = Endpoint::server(
            "127.0.0.1:0".parse().unwrap(),
            ServerConfig::new(server_tls).unwrap().early_data(true),
        )
        .unwrap();
        let addr: SocketAddr = endpoint.local_addr().unwrap();
        drop(giong::spawn(async move {
            Server::new().serve_h3(endpoint, App).await
        }));
        let quic = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        let cfg = ClientConfig::new(client_tls).unwrap().early_data(true);
        // Opens a request stream and sends `request` on it; the response
        // is read later.
        async fn send(conn: &Connection, request: Vec<u8>) -> giong_quic::RecvStream {
            let (mut send, recv) = conn.open_bi().await.unwrap();
            send.write_all(&request).await.unwrap();
            send.finish().unwrap();
            recv
        }
        let mut control_frame = vec![0x00];
        Settings::write_frame(&mut control_frame, 1 << 20);
        let mut post_hi = post("/echo", &[("content-length", "2")]);
        post_hi.extend_from_slice(&data(b"hi"));

        // The first connection is not early, and leaves a ticket.
        let first = quic.connect_early(addr, "localhost", &cfg).await.unwrap();
        let mut control = first.open_uni().await.unwrap();
        control.write_all(&control_frame).await.unwrap();
        let mut recv = send(&first, get("/early")).await;
        assert!(!recv.is_early());
        let reply = parse(&recv.read_to_end(1 << 16).await.unwrap());
        assert_eq!((reply.status(), &reply.body[..]), (200, &b"false"[..]));
        first.close(0x100, b"");
        drop((control, recv, first));

        // The second sends its requests before its handshake ends.
        let second = quic.connect_early(addr, "localhost", &cfg).await.unwrap();
        let mut control = second.open_uni().await.unwrap();
        control.write_all(&control_frame).await.unwrap();
        let mut early_get = send(&second, get("/early")).await;
        let mut early_post = send(&second, post_hi.clone()).await;
        assert!(early_get.is_early() && early_post.is_early());
        let reply = parse(&early_get.read_to_end(1 << 16).await.unwrap());
        assert_eq!((reply.status(), &reply.body[..]), (200, &b"true"[..]));
        let reply = parse(&early_post.read_to_end(1 << 16).await.unwrap());
        assert_eq!(reply.status(), 425);
        assert!(second.handshake().await.unwrap());

        // After the handshake, nothing is early: the POST goes through.
        let mut late = send(&second, get("/early")).await;
        assert!(!late.is_early());
        let reply = parse(&late.read_to_end(1 << 16).await.unwrap());
        assert_eq!(&reply.body[..], b"false");
        let mut late = send(&second, post_hi).await;
        let reply = parse(&late.read_to_end(1 << 16).await.unwrap());
        assert_eq!((reply.status(), &reply.body[..]), (200, &b"hi"[..]));
        drop(control);
    });
}
