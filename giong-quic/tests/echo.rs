//! giong-quic end to end: its own client against its own server, and
//! quinn (on Tokio, another thread) against a giong-quic server.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use giong_quic::{ClientConfig, Endpoint, ServerConfig};

const ALPN: &[u8] = b"giong-test";

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn server_tls() -> (
    rustls::ServerConfig,
    rustls::pki_types::CertificateDer<'static>,
) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key.into())
        .unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    (tls, der)
}

fn client_tls(ca: rustls::pki_types::CertificateDer<'static>) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls
}

/// Serves: every stream is answered with its own bytes reversed.
async fn serve(ep: Endpoint) {
    while let Some(conn) = ep.accept().await {
        giong::spawn(async move {
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                giong::spawn(async move {
                    let mut req = recv.read_to_end(4 << 20).await.unwrap();
                    req.reverse();
                    send.write_all(&req).await.unwrap();
                    send.finish().unwrap();
                });
            }
        });
    }
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn giong_client_and_server() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(serve(server));

            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            assert_eq!(conn.remote_addr(), addr);
            // Small and large streams, several at once.
            let mut tasks = Vec::new();
            let conn = std::rc::Rc::new(conn);
            for n in [0usize, 1, 1200, 70_000, 3 << 20] {
                let conn = conn.clone();
                tasks.push(giong::spawn(async move {
                    let (mut send, mut recv) = conn.open_bi().await.unwrap();
                    let data = payload(n);
                    send.write_all(&data).await.unwrap();
                    send.finish().unwrap();
                    let got = recv.read_to_end(4 << 20).await.unwrap();
                    let mut want = data;
                    want.reverse();
                    assert!(got == want, "stream of {n} bytes");
                }));
            }
            for t in tasks {
                giong::time::timeout(Duration::from_secs(20), t)
                    .await
                    .unwrap()
                    .unwrap();
            }
        })
        .unwrap();
}

#[test]
fn a_wrong_server_name_fails_with_its_code() {
    giong::Builder::new()
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(serve(server));
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let e = match client.connect(addr, "example.org", &cfg).await {
                Ok(_) => panic!("connected with a certificate for another name"),
                Err(e) => e,
            };
            assert_eq!(
                giong::error::code(&e),
                Some(giong::error::Code::QuicConnect),
                "{e}"
            );
        })
        .unwrap();
}

#[test]
fn a_config_without_alpn_is_refused() {
    let (mut tls, _) = server_tls();
    tls.alpn_protocols.clear();
    let e = ServerConfig::new(tls).err().unwrap();
    assert_eq!(
        giong::error::code(&e),
        Some(giong::error::Code::QuicConfig),
        "{e}"
    );
}

/// quinn on Tokio (its own thread) as the client of a giong-quic server.
#[test]
fn quinn_client_against_giong_server() {
    let (tls, ca) = server_tls();
    let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
    let server = std::thread::spawn(move || {
        giong::Builder::new()
            .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
            .block_on(async move {
                let ep = Endpoint::server(
                    "127.0.0.1:0".parse().unwrap(),
                    ServerConfig::new(tls).unwrap(),
                )
                .unwrap();
                tx.send(ep.local_addr().unwrap()).unwrap();
                let _ = giong::time::timeout(Duration::from_secs(30), serve(ep)).await;
            })
            .unwrap();
    });
    let addr = rx.recv().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_tls(ca)).unwrap();
        let mut ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        let conn = ep.connect(addr, "localhost").unwrap().await.unwrap();
        for n in [5usize, 100_000, 1 << 20] {
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let data = payload(n);
            send.write_all(&data).await.unwrap();
            send.finish().unwrap();
            let got = recv.read_to_end(4 << 20).await.unwrap();
            let mut want = data;
            want.reverse();
            assert!(got == want, "stream of {n} bytes");
        }
        conn.close(0u32.into(), b"done");
        ep.wait_idle().await;
    });
    drop(server);
}

/// Unidirectional streams both ways, a stream the sender resets and one the
/// receiver stops: each side sees the other's code as `GIONG-E3604`.
#[test]
fn unidirectional_streams_resets_and_stops() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            let served = giong::spawn(async move {
                let conn = server.accept().await.unwrap();
                // A one-way stream in, answered on a one-way stream out.
                let mut recv = conn.accept_uni().await.unwrap();
                assert_eq!(recv.id() & 3, 2, "client-initiated, unidirectional");
                let got = recv.read_to_end(1 << 20).await.unwrap();
                let mut send = conn.open_uni().await.unwrap();
                assert_eq!(send.id() & 3, 3, "server-initiated, unidirectional");
                send.write_all(&got).await.unwrap();
                send.write_all(b"!").await.unwrap();
                send.finish().unwrap();
                // The client resets the next one after its first bytes.
                let mut recv = conn.accept_uni().await.unwrap();
                let err = recv.read_to_end(1 << 20).await.unwrap_err();
                assert_eq!(
                    giong::error::code(&err),
                    Some(giong::error::Code::QuicStreamReset)
                );
                assert!(err.to_string().contains("code 77"), "{err}");
                // And we stop reading the one after: its writer learns why.
                let mut recv = conn.accept_uni().await.unwrap();
                let first = recv.read().await.unwrap().expect("some bytes");
                assert!(first.starts_with(b"start"));
                recv.stop(99);
                // Keep the connection until the client is done.
                let _ = conn.accept_uni().await;
            });

            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            let mut send = conn.open_uni().await.unwrap();
            send.write_all(b"one way").await.unwrap();
            send.finish().unwrap();
            let mut recv = conn.accept_uni().await.unwrap();
            assert_eq!(recv.read_to_end(100).await.unwrap(), b"one way!");

            let mut send = conn.open_uni().await.unwrap();
            send.write_all(b"never finished").await.unwrap();
            send.reset(77);

            let mut send = conn.open_uni().await.unwrap();
            send.write_all(b"start").await.unwrap();
            let chunk = vec![0u8; 64 * 1024];
            let err = giong::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Err(e) = send.write_all(&chunk).await {
                        break e;
                    }
                }
            })
            .await
            .expect("the writer is told to stop");
            assert_eq!(
                giong::error::code(&err),
                Some(giong::error::Code::QuicStreamReset)
            );
            assert!(err.to_string().contains("code 99"), "{err}");
            drop(conn);
            let _ = giong::time::timeout(Duration::from_secs(5), served).await;
        })
        .unwrap();
}

/// A `connect` whose future is dropped (here by a timeout: nothing listens
/// on the port) closes its connection instead of leaving it behind.
#[test]
fn a_dropped_connect_leaves_no_connection() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            // A bound socket that never answers.
            let silent = giong::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let to = silent.local_addr().unwrap();
            let (_, ca) = server_tls();
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let attempt = client.connect(to, "localhost", &cfg);
            assert!(giong::time::timeout(Duration::from_millis(100), attempt)
                .await
                .is_err());
            // Closed at once; forgotten when its draining period is over
            // (three probe timeouts, about three seconds before any RTT
            // sample).
            let gone = giong::time::timeout(Duration::from_secs(20), async {
                while client.connections() > 0 {
                    giong::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            assert!(gone.is_ok(), "{} connections left", client.connections());
        })
        .unwrap();
}
