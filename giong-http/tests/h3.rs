//! `Server::serve_h3` against an HTTP/3 client that is not giong's own:
//! the `h3` crate over quinn, on Tokio in this thread, with the giong
//! server on another.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use giong_http::{Body, Handler, Request, Response, Server, Version};

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

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + i / 253) as u8).collect()
}

struct App;

impl Handler for App {
    async fn call(&self, req: Request) -> Response {
        assert_eq!(req.version(), Version::Http3);
        match req.path() {
            "/" => Response::text("hello").header("x-method", req.method()),
            "/echo" => Response::new(200).body(req.body().to_vec()),
            "/big" => Response::new(200).body(pattern(3 << 20)),
            "/count" => {
                let body = req.body_stream().expect("a streamed body");
                let mut len = 0;
                while let Ok(Some(piece)) = body.chunk().await {
                    len += piece.len();
                }
                Response::text(len.to_string())
            }
            "/events" => {
                let (tx, body) = Body::channel();
                drop(giong::spawn(async move {
                    for i in 0..20 {
                        if tx.send(format!("event {i}\n")).await.is_err() {
                            return;
                        }
                        giong::time::sleep(Duration::from_millis(1)).await;
                    }
                }));
                Response::new(200)
                    .header("content-type", "text/event-stream")
                    .body(body)
            }
            _ => Response::new(404),
        }
    }

    fn stream_body(&self, req: &Request) -> Option<u64> {
        (req.path() == "/count").then_some(64 << 20)
    }
}

type Sender = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;

/// One request with `body`; the status, one header and the body of the
/// response.
async fn fetch(
    mut sender: Sender,
    method: &str,
    path: &str,
    body: Option<Vec<u8>>,
) -> (u16, http::HeaderMap, Vec<u8>) {
    let request = http::Request::builder()
        .method(method)
        .uri(format!("https://localhost{path}"))
        .header("user-agent", "giong-test")
        .body(())
        .unwrap();
    let mut stream = sender.send_request(request).await.unwrap();
    if let Some(body) = body {
        for piece in body.chunks(100_000) {
            stream
                .send_data(Bytes::copy_from_slice(piece))
                .await
                .unwrap();
        }
    }
    stream.finish().await.unwrap();
    let response = stream.recv_response().await.unwrap();
    let mut got = Vec::new();
    while let Some(mut chunk) = stream.recv_data().await.unwrap() {
        while chunk.has_remaining() {
            let part = chunk.chunk();
            got.extend_from_slice(part);
            let n = part.len();
            chunk.advance(n);
        }
    }
    (response.status().as_u16(), response.headers().clone(), got)
}

#[test]
fn the_h3_crate_talks_to_serve_h3() {
    let (server_tls, client_tls) = tls();
    let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
    let server = std::thread::spawn(move || {
        giong::Builder::new()
            .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
            .block_on(async move {
                let endpoint = giong_quic::Endpoint::server(
                    "127.0.0.1:0".parse().unwrap(),
                    giong_quic::ServerConfig::new(server_tls).unwrap(),
                )
                .unwrap();
                tx.send(endpoint.local_addr().unwrap()).unwrap();
                let server = Server::new();
                let serve = server.serve_h3(endpoint, App);
                let _ = giong::time::timeout(Duration::from_secs(60), serve).await;
            })
            .unwrap();
    });
    let addr = rx.recv().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap();
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
        let (mut driver, sender) = h3::client::new(h3_quinn::Connection::new(conn))
            .await
            .unwrap();
        // The driver reads the server's control stream: an error there (a
        // setting or a frame the client refuses) ends the test.
        let drive = tokio::spawn(async move {
            let end = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
            end.to_string()
        });

        let (status, headers, body) = fetch(sender.clone(), "GET", "/", None).await;
        assert_eq!((status, &body[..]), (200, &b"hello"[..]));
        assert_eq!(headers["x-method"], "GET");
        assert_eq!(headers["content-length"], "5");
        assert!(headers.contains_key("date"));

        let (status, headers, body) = fetch(sender.clone(), "HEAD", "/", None).await;
        assert_eq!((status, body.len()), (200, 0));
        assert_eq!(headers["content-length"], "5");
        let (status, ..) = fetch(sender.clone(), "GET", "/nowhere", None).await;
        assert_eq!(status, 404);

        let sent = pattern(700_000);
        let (status, _, body) = fetch(sender.clone(), "POST", "/echo", Some(sent.clone())).await;
        assert_eq!(status, 200);
        assert!(body == sent, "the echoed body differs");

        let (status, _, body) =
            fetch(sender.clone(), "PUT", "/count", Some(pattern(5 << 20))).await;
        assert_eq!((status, &body[..]), (200, &b"5242880"[..]));

        let (status, headers, body) = fetch(sender.clone(), "GET", "/events", None).await;
        assert_eq!(status, 200);
        assert!(!headers.contains_key("content-length"));
        let want: String = (0..20).map(|i| format!("event {i}\n")).collect();
        assert_eq!(String::from_utf8(body).unwrap(), want);

        // Many at once, large responses among them.
        let mut tasks = Vec::new();
        for i in 0..32 {
            let sender = sender.clone();
            tasks.push(tokio::spawn(async move {
                if i % 8 == 0 {
                    let (status, _, body) = fetch(sender, "GET", "/big", None).await;
                    assert_eq!(status, 200);
                    assert!(body == pattern(3 << 20), "the large body differs");
                } else {
                    let sent = pattern(1000 + i);
                    let (status, _, body) =
                        fetch(sender, "POST", "/echo", Some(sent.clone())).await;
                    assert_eq!(status, 200);
                    assert!(body == sent);
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert!(!drive.is_finished(), "the client closed the connection");
        drop(sender);
        let end = tokio::time::timeout(Duration::from_secs(5), drive)
            .await
            .expect("the connection closes once its last request sender is dropped")
            .unwrap();
        // h3 closes with H3_NO_ERROR when it is done.
        assert!(end.contains("NO_ERROR") || end.contains("closed"), "{end}");
        endpoint.wait_idle().await;
    });
    drop(server);
}

/// A client that allows the server no unidirectional stream, and keeps
/// the connection alive with PINGs, is closed after the write timeout
/// instead of holding it forever: the server cannot open its control
/// stream (RFC 9114 section 6.2 asks for three).
#[test]
fn a_client_without_credit_for_the_control_stream_is_closed() {
    let (server_tls, client_tls) = tls();
    let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
    std::thread::spawn(move || {
        giong::Builder::new()
            .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
            .block_on(async move {
                let endpoint = giong_quic::Endpoint::server(
                    "127.0.0.1:0".parse().unwrap(),
                    giong_quic::ServerConfig::new(server_tls).unwrap(),
                )
                .unwrap();
                tx.send(endpoint.local_addr().unwrap()).unwrap();
                let server = Server::new().write_timeout(Duration::from_millis(300));
                let serve = server.serve_h3(endpoint, App);
                let _ = giong::time::timeout(Duration::from_secs(20), serve).await;
            })
            .unwrap();
    });
    let addr = rx.recv().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap();
        let mut transport = quinn::TransportConfig::default();
        transport
            .max_concurrent_uni_streams(0u32.into())
            .keep_alive_interval(Some(Duration::from_millis(100)));
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        config.transport_config(Arc::new(transport));
        let mut endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        endpoint.set_default_client_config(config);
        let conn = endpoint.connect(addr, "localhost").unwrap().await.unwrap();
        let why = tokio::time::timeout(Duration::from_secs(5), conn.closed())
            .await
            .expect("the server held the connection");
        // H3_EXCESSIVE_LOAD.
        match why {
            quinn::ConnectionError::ApplicationClosed(close) => {
                assert_eq!(close.error_code.into_inner(), 0x107, "{close:?}");
            }
            other => panic!("{other}"),
        }
    });
}
