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
