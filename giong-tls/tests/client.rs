//! The client side: giong's `TlsConnector` against giong's own server, against
//! a blocking rustls server, and against servers it must refuse.

use std::cell::RefCell;
use std::future::Future;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener as StdListener};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream};
use giong_tls::rustls::pki_types::pem::PemObject;
use giong_tls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use giong_tls::rustls::server::WebPkiClientVerifier;
use giong_tls::rustls::{self, RootCertStore, ServerConfig, ServerConnection, StreamOwned};
use giong_tls::{TlsAcceptor, TlsConnector, TlsStream};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};

/// A certificate authority, a server certificate it issued for `localhost`
/// and `127.0.0.1`, and a client certificate it issued.
struct Pki {
    ca: String,
    server_cert: String,
    server_key: String,
    client_cert: String,
    client_key: String,
}

fn pki() -> Pki {
    pki_named("giong test CA")
}

fn pki_named(ca_name: &str) -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name.push(DnType::CommonName, ca_name);
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca.self_signed(&ca_key).unwrap();

    let server_key = KeyPair::generate().unwrap();
    let mut server = CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_cert = server.signed_by(&server_key, &ca_cert, &ca_key).unwrap();

    let client_key = KeyPair::generate().unwrap();
    let mut client = CertificateParams::new(Vec::<String>::new()).unwrap();
    client
        .distinguished_name
        .push(DnType::CommonName, "giong test client");
    client.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_cert = client.signed_by(&client_key, &ca_cert, &ca_key).unwrap();

    Pki {
        ca: ca_cert.pem(),
        server_cert: server_cert.pem(),
        server_key: server_key.serialize_pem(),
        client_cert: client_cert.pem(),
        client_key: client_key.serialize_pem(),
    }
}

fn der(pem: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(pem.as_bytes()).unwrap()
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A server configuration for `pki`'s server certificate, limited to
/// `versions`, optionally requiring a client certificate from `pki`'s CA.
fn server_config(
    pki: &Pki,
    versions: &[&'static rustls::SupportedProtocolVersion],
    require_client_cert: bool,
) -> Arc<ServerConfig> {
    let builder = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(versions)
        .unwrap();
    let builder = if require_client_cert {
        let mut roots = RootCertStore::empty();
        roots.add(der(&pki.ca)).unwrap();
        let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
            .build()
            .unwrap();
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };
    let key = PrivateKeyDer::from_pem_slice(pki.server_key.as_bytes()).unwrap();
    Arc::new(
        builder
            .with_single_cert(vec![der(&pki.server_cert)], key)
            .unwrap(),
    )
}

/// Accepts TLS connections on a new port of this worker and hands each
/// handshake's outcome to `serve`. Call from inside the runtime.
fn serve<F, Fut>(acceptor: TlsAcceptor, serve: F) -> SocketAddr
where
    F: Fn(io::Result<TlsStream>) -> Fut + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move {
        while let Ok(tcp) = listener.accept().await {
            let acceptor = acceptor.clone();
            let conn = async move { acceptor.accept(tcp).await };
            let handler = &serve;
            let fut = handler(conn.await);
            giong::spawn(fut);
        }
    });
    addr
}

async fn echo(tls: io::Result<TlsStream>) {
    let Ok(tls) = tls else { return };
    while let Ok(data) = tls.recv().await {
        if data.is_empty() {
            break;
        }
        if tls.write_all(data).await.0.is_err() {
            return;
        }
    }
    let _ = tls.shutdown().await;
}

async fn connect(connector: &TlsConnector, addr: SocketAddr, name: &str) -> io::Result<TlsStream> {
    let tcp = TcpStream::connect(addr).await?;
    connector.connect(name, tcp).await
}

/// Receives exactly `n` bytes.
async fn recv_exact(tls: &TlsStream, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let data = tls.recv().await.unwrap();
        assert!(!data.is_empty(), "closed after {} of {n} bytes", out.len());
        out.extend_from_slice(&data);
    }
    assert_eq!(out.len(), n, "more data than expected");
    out
}

fn roundtrip(versions: &[&'static rustls::SupportedProtocolVersion]) {
    let pki = pki();
    let acceptor = TlsAcceptor::new(server_config(&pki, versions, false));
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, echo);
        let tls = Rc::new(connect(&connector, addr, "localhost").await.unwrap());
        assert_eq!(tls.peer_certificates().unwrap()[0], der(&pki.server_cert));

        tls.write_all(b"hello over tls".to_vec()).await.0.unwrap();
        assert_eq!(recv_exact(&tls, 14).await, b"hello over tls");

        // 4 MB written by one task while another reads the echo.
        let data: Vec<u8> = (0..4_000_000u32).map(|i| (i % 251) as u8).collect();
        let writer = giong::spawn({
            let (tls, data) = (tls.clone(), data.clone());
            async move {
                for piece in data.chunks(65_536) {
                    tls.write_all(piece.to_vec()).await.0.unwrap();
                }
            }
        });
        let back = recv_exact(&tls, data.len()).await;
        assert!(back == data, "the echo differs from what was sent");
        writer.await.unwrap();

        // close_notify both ways.
        tls.shutdown().await.unwrap();
        assert!(tls.recv().await.unwrap().is_empty());
        assert!(tls.recv().await.unwrap().is_empty());
    });
}

#[test]
fn talks_to_a_giong_server_over_tls_1_3() {
    roundtrip(&[&rustls::version::TLS13]);
}

#[test]
fn talks_to_a_giong_server_over_tls_1_2() {
    roundtrip(&[&rustls::version::TLS12]);
}

#[test]
fn talks_to_a_blocking_rustls_server() {
    let pki = pki();
    let config = server_config(
        &pki,
        &[&rustls::version::TLS13, &rustls::version::TLS12],
        false,
    );
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut tls = StreamOwned::new(ServerConnection::new(config).unwrap(), tcp);
        let mut hello = [0u8; 5];
        tls.read_exact(&mut hello).unwrap();
        assert_eq!(&hello, b"hello");
        tls.write_all(b"world").unwrap();
        tls.conn.send_close_notify();
        tls.flush().unwrap();
    });
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let tls = connect(&connector, addr, "localhost").await.unwrap();
        tls.write_all(b"hello".to_vec()).await.0.unwrap();
        assert_eq!(recv_exact(&tls, 5).await, b"world");
        assert!(tls.recv().await.unwrap().is_empty());
    });
    server.join().unwrap();
}

#[test]
fn a_certificate_for_another_name_is_refused() {
    let pki = pki();
    let acceptor =
        TlsAcceptor::from_pem(pki.server_cert.as_bytes(), pki.server_key.as_bytes()).unwrap();
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, echo);
        let err = connect(&connector, addr, "example.com").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("not valid for name"), "{err}");
    });
}

#[test]
fn a_certificate_from_an_unknown_authority_is_refused() {
    let pki = pki();
    let stranger = pki_named("some other CA");
    let acceptor =
        TlsAcceptor::from_pem(pki.server_cert.as_bytes(), pki.server_key.as_bytes()).unwrap();
    let connector = TlsConnector::with_root_pem(stranger.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, echo);
        let err = connect(&connector, addr, "localhost").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        assert!(err.to_string().contains("UnknownIssuer"), "{err}");
    });
}

#[test]
fn an_ip_address_can_be_the_server_name() {
    let pki = pki();
    let acceptor =
        TlsAcceptor::from_pem(pki.server_cert.as_bytes(), pki.server_key.as_bytes()).unwrap();
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, echo);
        let tls = connect(&connector, addr, "127.0.0.1").await.unwrap();
        tls.write_all(b"ip".to_vec()).await.0.unwrap();
        assert_eq!(recv_exact(&tls, 2).await, b"ip");
        // The certificate is for 127.0.0.1, not for another address.
        let err = connect(&connector, addr, "127.0.0.2").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    });
}

#[test]
fn a_server_name_that_is_not_a_name_is_refused_before_the_handshake() {
    let pki = pki();
    let acceptor =
        TlsAcceptor::from_pem(pki.server_cert.as_bytes(), pki.server_key.as_bytes()).unwrap();
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, echo);
        for name in ["not a name", "", "exa mple.com", "[nope]"] {
            let err = connect(&connector, addr, name).await.unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{name:?}: {err}");
        }
    });
}

#[test]
fn alpn_settles_on_a_protocol_both_sides_speak() {
    let pki = pki();
    // The server offers http/1.1 only.
    let acceptor =
        TlsAcceptor::from_pem(pki.server_cert.as_bytes(), pki.server_key.as_bytes()).unwrap();
    let plain = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    let alpn = plain.clone().alpn(&[b"h2", b"http/1.1"]);
    giong::block_on(async move {
        let server_side = Rc::new(RefCell::new(Vec::new()));
        let seen = server_side.clone();
        let addr = serve(acceptor, move |tls| {
            if let Ok(tls) = &tls {
                seen.borrow_mut().push(tls.alpn_protocol());
            }
            echo(tls)
        });
        let tls = connect(&alpn, addr, "localhost").await.unwrap();
        assert_eq!(tls.alpn_protocol().as_deref(), Some(&b"http/1.1"[..]));
        let tls = connect(&plain, addr, "localhost").await.unwrap();
        assert_eq!(tls.alpn_protocol(), None);
        tls.write_all(b"x".to_vec()).await.0.unwrap();
        recv_exact(&tls, 1).await;
        assert_eq!(
            *server_side.borrow(),
            [Some(b"http/1.1".to_vec()), None],
            "what the server agreed on"
        );
    });
}

#[test]
fn a_client_certificate_is_presented_when_the_server_asks_for_one() {
    let pki = pki();
    let versions = [&rustls::version::TLS13, &rustls::version::TLS12];
    let acceptor = TlsAcceptor::new(server_config(&pki, &versions, true));
    let anonymous = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    let identified = anonymous
        .clone()
        .client_auth(pki.client_cert.as_bytes(), pki.client_key.as_bytes())
        .unwrap();
    let client_der = der(&pki.client_cert);
    giong::block_on(async move {
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let seen = outcomes.clone();
        let addr = serve(acceptor, move |tls| {
            seen.borrow_mut().push(match &tls {
                Ok(tls) => Ok(tls.peer_certificates().map(|c| c[0].clone())),
                Err(e) => Err(e.kind()),
            });
            echo(tls)
        });

        let tls = connect(&identified, addr, "localhost").await.unwrap();
        tls.write_all(b"me".to_vec()).await.0.unwrap();
        assert_eq!(recv_exact(&tls, 2).await, b"me");
        assert_eq!(outcomes.borrow()[0], Ok(Some(client_der)));

        // Without one, the server refuses. With TLS 1.3 the client's side of
        // the handshake is over before the server has checked, so the refusal
        // may only show on the first receive.
        let refused = async {
            let tls = connect(&anonymous, addr, "localhost").await?;
            tls.write_all(b"me".to_vec()).await.0?;
            tls.recv().await
        }
        .await;
        let err = refused.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        // The server's side may finish a moment after the client's.
        for _ in 0..100 {
            if outcomes.borrow().len() == 2 {
                break;
            }
            giong::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(outcomes.borrow()[1], Err(io::ErrorKind::InvalidData));
    });
}

#[test]
fn a_client_certificate_must_match_its_key() {
    let pki = pki();
    let other = self::pki();
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    let err = connector
        .client_auth(pki.client_cert.as_bytes(), other.client_key.as_bytes())
        .unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
}

#[test]
fn reconnecting_resumes_the_session() {
    let pki = pki();
    let acceptor =
        TlsAcceptor::from_pem(pki.server_cert.as_bytes(), pki.server_key.as_bytes()).unwrap();
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, echo);
        let first = connect(&connector, addr, "localhost").await.unwrap();
        assert!(!first.resumed());
        // The session tickets arrive after the handshake, with the data.
        first.write_all(b"1".to_vec()).await.0.unwrap();
        recv_exact(&first, 1).await;
        // A clone shares the session cache.
        let second = connect(&connector.clone(), addr, "localhost")
            .await
            .unwrap();
        assert!(second.resumed());
        second.write_all(b"2".to_vec()).await.0.unwrap();
        assert_eq!(recv_exact(&second, 1).await, b"2");
    });
}

#[test]
fn a_server_that_never_answers_times_out() {
    // Connections queue in the backlog; nobody ever accepts them.
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let pki = pki();
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes())
        .unwrap()
        .handshake_timeout(Duration::from_millis(200));
    giong::block_on(async move {
        let started = Instant::now();
        let err = connect(&connector, addr, "localhost").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut, "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    });
    drop(listener);
}

#[test]
fn a_server_that_hangs_up_during_the_handshake_is_an_error() {
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        // Read the whole ClientHello record, so that closing sends a FIN
        // rather than a reset, then hang up without answering.
        let (mut tcp, _) = listener.accept().unwrap();
        let mut header = [0u8; 5];
        tcp.read_exact(&mut header).unwrap();
        let len = u16::from_be_bytes([header[3], header[4]]) as usize;
        let mut hello = vec![0u8; len];
        tcp.read_exact(&mut hello).unwrap();
    });
    let pki = pki();
    let connector = TlsConnector::with_root_pem(pki.ca.as_bytes()).unwrap();
    giong::block_on(async move {
        let err = connect(&connector, addr, "localhost").await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof, "{err}");
    });
    server.join().unwrap();
}

#[test]
fn trust_roots_must_be_certificates() {
    let err = TlsConnector::with_root_pem(b"no pem here").unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    let err = TlsConnector::with_roots(RootCertStore::empty()).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
}
