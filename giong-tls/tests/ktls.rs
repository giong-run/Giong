//! Kernel TLS: connections whose records the kernel encrypts once rustls has
//! done the handshake.
//!
//! Most tests need a kernel with the `tls` module loaded (`modprobe tls`).
//! Without one they print why and pass, unless `GIONG_KTLS_REQUIRED` is set,
//! as in CI, where a missing module is a failure. The tests of what happens
//! without kTLS run either way.

use std::future::Future;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use giong::error::Code;
use giong::net::{TcpListener, TcpStream};
use giong::time::{sleep, timeout};
use giong_tls::rustls::crypto::ring::cipher_suite as suites;
use giong_tls::rustls::crypto::CryptoProvider;
use giong_tls::rustls::pki_types::pem::PemObject;
use giong_tls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use giong_tls::rustls::{
    self, ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection,
    StreamOwned, SupportedCipherSuite,
};
use giong_tls::{ktls_stats, ktls_support, Ktls, TlsAcceptor, TlsConnector, TlsStream};

// ------------------------------------------------------------------ setup

/// `true` if this kernel does kTLS. Otherwise the test says why and passes,
/// or fails if `GIONG_KTLS_REQUIRED` is set.
fn kernel_tls() -> bool {
    let support = ktls_support();
    if support.is_available() {
        return true;
    }
    assert!(
        std::env::var_os("GIONG_KTLS_REQUIRED").is_none(),
        "GIONG_KTLS_REQUIRED is set, but {support}"
    );
    eprintln!("skipped: {support}");
    false
}

/// `true` if the kernel also takes new TLS 1.3 keys (Linux 6.14+).
fn kernel_key_updates() -> bool {
    let ok = ktls_support().key_update();
    if !ok {
        eprintln!("this kernel cannot change TLS 1.3 keys: {}", ktls_support());
    }
    ok
}

struct Pki {
    cert_pem: String,
    key_pem: String,
}

fn pki() -> Pki {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    Pki {
        cert_pem: key.cert.pem(),
        key_pem: key.key_pair.serialize_pem(),
    }
}

impl Pki {
    fn cert(&self) -> CertificateDer<'static> {
        CertificateDer::from_pem_slice(self.cert_pem.as_bytes()).unwrap()
    }

    fn roots(&self) -> RootCertStore {
        let mut roots = RootCertStore::empty();
        roots.add(self.cert()).unwrap();
        roots
    }

    /// A server configuration allowing `suite` only, or rustls' defaults.
    fn server(&self, suite: Option<SupportedCipherSuite>) -> ServerConfig {
        let key = PrivateKeyDer::from_pem_slice(self.key_pem.as_bytes()).unwrap();
        ServerConfig::builder_with_provider(provider(suite))
            .with_protocol_versions(&versions(suite))
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![self.cert()], key)
            .unwrap()
    }

    /// A client configuration allowing `suite` only, or rustls' defaults.
    fn client(&self, suite: Option<SupportedCipherSuite>) -> ClientConfig {
        ClientConfig::builder_with_provider(provider(suite))
            .with_protocol_versions(&versions(suite))
            .unwrap()
            .with_root_certificates(self.roots())
            .with_no_client_auth()
    }

    fn acceptor(&self, suite: Option<SupportedCipherSuite>, mode: Ktls) -> TlsAcceptor {
        TlsAcceptor::new(Arc::new(self.server(suite)))
            .ktls(mode)
            .unwrap()
    }

    fn connector(&self, suite: Option<SupportedCipherSuite>, mode: Ktls) -> TlsConnector {
        TlsConnector::new(Arc::new(self.client(suite)))
            .ktls(mode)
            .unwrap()
    }
}

fn provider(suite: Option<SupportedCipherSuite>) -> Arc<CryptoProvider> {
    let mut provider = rustls::crypto::ring::default_provider();
    if let Some(suite) = suite {
        provider.cipher_suites = vec![suite];
    }
    Arc::new(provider)
}

fn versions(suite: Option<SupportedCipherSuite>) -> Vec<&'static rustls::SupportedProtocolVersion> {
    match suite {
        Some(suite) => vec![suite.version()],
        None => rustls::DEFAULT_VERSIONS.to_vec(),
    }
}

/// The suites rustls' `ring` provider shares with the kernel. rcgen's keys
/// are ECDSA.
const SUITES: [SupportedCipherSuite; 6] = [
    suites::TLS13_AES_128_GCM_SHA256,
    suites::TLS13_AES_256_GCM_SHA384,
    suites::TLS13_CHACHA20_POLY1305_SHA256,
    suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    suites::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    suites::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
];

fn tls13(suite: SupportedCipherSuite) -> bool {
    suite.version() == &rustls::version::TLS13
}

/// Runs `f` on a fresh single-worker runtime and gives back its result.
fn run<F: Future + 'static>(f: F) -> F::Output {
    giong::block_on(f)
}

/// A connected pair: what `acceptor` and `connector` made of one loopback
/// connection.
async fn tls_pair(acceptor: TlsAcceptor, connector: TlsConnector) -> (TlsStream, TlsStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = giong::spawn(async move {
        let tcp = listener.accept().await.unwrap();
        acceptor.accept(tcp).await
    });
    let tcp = TcpStream::connect(addr).await.unwrap();
    let client = connector.connect("localhost", tcp).await.unwrap();
    let server = server.await.unwrap().unwrap();
    (server, client)
}

/// Receives exactly `n` bytes.
async fn recv_exactly(tls: &TlsStream, n: usize) -> Vec<u8> {
    let mut got = Vec::with_capacity(n);
    while got.len() < n {
        let data = timeout(Duration::from_secs(20), tls.recv())
            .await
            .expect("data within 20 s")
            .unwrap();
        assert!(!data.is_empty(), "closed after {} of {n} bytes", got.len());
        got.extend_from_slice(&data);
    }
    assert_eq!(got.len(), n, "more than asked for");
    got
}

/// Messages of awkward sizes around the 16 KiB record size, then a few
/// megabytes, from `a` to `b` and back; then a header, a body and a trailer
/// in one vectored write each way.
async fn exchange(a: &TlsStream, b: &TlsStream) {
    let sizes = [1, 2, 100, 16383, 16384, 16385, 32768, 65537, 1_000_000];
    for (i, &n) in sizes.iter().enumerate() {
        let data: Vec<u8> = (0..n).map(|j| (j * 7 + i) as u8).collect();
        let (res, data) = a.write_all(data).await;
        res.unwrap();
        assert!(recv_exactly(b, n).await == data, "{n} bytes a to b");
        let (res, data) = b.write_all(data).await;
        res.unwrap();
        assert!(recv_exactly(a, n).await == data, "{n} bytes b to a");
    }
    let body: Vec<u8> = (0..150_000u32).map(|j| (j % 251) as u8).collect();
    let pieces = vec![b"head ".to_vec(), body, Vec::new(), b" tail".to_vec()];
    let whole = pieces.concat();
    for (from, to) in [(a, b), (b, a)] {
        let (res, back) = from.write_all_vectored(pieces.clone()).await;
        res.unwrap();
        assert!(back == pieces, "the pieces are handed back");
        assert!(
            recv_exactly(to, whole.len()).await == whole,
            "vectored write"
        );
    }
}

/// Both sides close with `close_notify`, and each sees the other's.
async fn close_both(a: &TlsStream, b: &TlsStream) {
    a.shutdown().await.unwrap();
    assert!(recv_exactly_end(b).await);
    b.shutdown().await.unwrap();
    assert!(recv_exactly_end(a).await);
}

/// Waits for the end of the stream; `true` if it ended with `close_notify`.
async fn recv_exactly_end(tls: &TlsStream) -> bool {
    let data = timeout(Duration::from_secs(20), tls.recv())
        .await
        .expect("the end within 20 s")
        .unwrap();
    assert!(data.is_empty(), "{} bytes instead of the end", data.len());
    tls.received_close_notify()
}

fn code(e: &std::io::Error) -> Option<Code> {
    giong::error::code(e)
}

// ------------------------------------------------- without kernel support

#[test]
fn auto_stays_in_the_process_when_the_kernel_cannot_take_connections() {
    if ktls_support().is_available() {
        eprintln!("skipped: this kernel has kTLS");
        return;
    }
    let pki = pki();
    let acceptor = pki.acceptor(None, Ktls::Auto);
    let connector = pki.connector(None, Ktls::Auto);
    let why = acceptor.ktls_unavailable().expect("a reason").to_string();
    assert_eq!(connector.ktls_unavailable(), Some(why.as_str()));
    assert_eq!(why, ktls_support().unavailable_reason().unwrap());
    let before = ktls_stats();
    run(async move {
        let (server, client) = tls_pair(acceptor, connector).await;
        assert!(!server.is_ktls() && !client.is_ktls());
        exchange(&server, &client).await;
        close_both(&server, &client).await;
    });
    let after = ktls_stats();
    assert!(after.stayed_in_process >= before.stayed_in_process + 2);
}

#[test]
fn required_fails_up_front_and_says_why_when_the_kernel_cannot() {
    if ktls_support().is_available() {
        eprintln!("skipped: this kernel has kTLS");
        return;
    }
    let pki = pki();
    let err = TlsAcceptor::new(Arc::new(pki.server(None)))
        .ktls(Ktls::Required)
        .unwrap_err();
    assert_eq!(code(&err), Some(Code::KtlsUnavailable), "{err}");
    let why = ktls_support().unavailable_reason().unwrap();
    assert!(err.to_string().contains(why), "{err}");
    let err = TlsConnector::new(Arc::new(pki.client(None)))
        .ktls(Ktls::Required)
        .unwrap_err();
    assert_eq!(code(&err), Some(Code::KtlsUnavailable), "{err}");
}

#[test]
fn configurations_with_0rtt_data_cannot_use_kernel_tls() {
    let pki = pki();
    let mut server = pki.server(None);
    server.max_early_data_size = 16384;
    let acceptor = TlsAcceptor::new(Arc::new(server.clone()))
        .ktls(Ktls::Auto)
        .unwrap();
    let why = acceptor.ktls_unavailable().expect("a reason");
    assert!(why.contains("0-RTT"), "{why}");
    let err = TlsAcceptor::new(Arc::new(server))
        .ktls(Ktls::Required)
        .unwrap_err();
    assert_eq!(code(&err), Some(Code::KtlsUnavailable), "{err}");

    let mut client = pki.client(None);
    client.enable_early_data = true;
    let err = TlsConnector::new(Arc::new(client))
        .ktls(Ktls::Required)
        .unwrap_err();
    assert!(err.to_string().contains("0-RTT"), "{err}");
}

#[test]
fn off_is_the_default_and_keeps_connections_in_the_process() {
    let pki = pki();
    let acceptor = TlsAcceptor::new(Arc::new(pki.server(None)));
    let connector = TlsConnector::new(Arc::new(pki.client(None)));
    assert_eq!(acceptor.ktls_unavailable(), None);
    run(async move {
        let (server, client) = tls_pair(acceptor, connector).await;
        assert!(!server.is_ktls() && !client.is_ktls());
        close_both(&server, &client).await;
    });
}

// ---------------------------------------------------- with kernel support

#[test]
fn every_cipher_suite_goes_through_the_kernel_in_both_roles() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    for suite in SUITES {
        if !ktls_support().supports(suite.suite()) {
            eprintln!("this kernel lacks {:?}: {}", suite.suite(), ktls_support());
            continue;
        }
        // Kernel server and process client, process server and kernel
        // client, kernel on both sides.
        for (server_mode, client_mode) in [
            (Ktls::Required, Ktls::Off),
            (Ktls::Off, Ktls::Required),
            (Ktls::Required, Ktls::Required),
        ] {
            let acceptor = pki.acceptor(Some(suite), server_mode);
            let connector = pki.connector(Some(suite), client_mode);
            let before = ktls_stats();
            run(async move {
                let (server, client) = tls_pair(acceptor, connector).await;
                let label = format!("{:?} {server_mode:?}/{client_mode:?}", suite.suite());
                assert_eq!(server.is_ktls(), server_mode == Ktls::Required, "{label}");
                assert_eq!(client.is_ktls(), client_mode == Ktls::Required, "{label}");
                exchange(&server, &client).await;
                exchange(&client, &server).await;
                close_both(&client, &server).await;
            });
            let offloaded = [server_mode, client_mode]
                .iter()
                .filter(|m| **m == Ktls::Required)
                .count() as u64;
            assert!(ktls_stats().offloaded >= before.offloaded + offloaded);
        }
    }
}

#[test]
fn auto_takes_the_kernel_when_it_can_do_everything() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    let acceptor = pki.acceptor(None, Ktls::Auto);
    let connector = pki.connector(None, Ktls::Auto);
    // The default configuration allows TLS 1.3, which needs key updates.
    let expected = ktls_support().key_update();
    assert_eq!(acceptor.ktls_unavailable().is_none(), expected);
    run(async move {
        let (server, client) = tls_pair(acceptor, connector).await;
        assert_eq!(server.is_ktls(), expected);
        assert_eq!(client.is_ktls(), expected);
        exchange(&server, &client).await;
        close_both(&server, &client).await;
    });
    // TLS 1.2 only needs no key updates.
    let suite = suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256;
    let acceptor = pki.acceptor(Some(suite), Ktls::Auto);
    assert_eq!(acceptor.ktls_unavailable(), None);
}

#[test]
fn what_the_handshake_settled_survives_the_handoff() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    for suite in [
        suites::TLS13_AES_128_GCM_SHA256,
        suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    ] {
        let mut server = pki.server(Some(suite));
        server.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::new(Arc::new(server))
            .ktls(Ktls::Required)
            .unwrap();
        let connector = pki
            .connector(Some(suite), Ktls::Required)
            .alpn(&[b"http/1.1"]);
        let cert = pki.cert();
        run(async move {
            // First a full handshake, then one resuming it: TLS 1.3 tickets
            // arrive after the handshake, through the kernel, and must reach
            // the client's session cache.
            for round in 0..2 {
                let (server, client) = tls_pair(acceptor.clone(), connector.clone()).await;
                assert!(server.is_ktls() && client.is_ktls());
                assert_eq!(server.alpn_protocol().as_deref(), Some(&b"http/1.1"[..]));
                assert_eq!(client.alpn_protocol().as_deref(), Some(&b"http/1.1"[..]));
                assert_eq!(client.peer_certificates().unwrap()[0], cert);
                assert_eq!(server.peer_certificates(), None);
                assert_eq!(
                    client.resumed(),
                    round == 1,
                    "{:?} round {round}",
                    suite.suite()
                );
                assert_eq!(server.resumed(), round == 1);
                exchange(&server, &client).await;
                close_both(&server, &client).await;
            }
        });
    }
}

#[test]
fn data_sent_with_the_last_handshake_flight_is_not_lost() {
    if !kernel_tls() {
        return;
    }
    // A rustls peer that queues data before the handshake is over sends it
    // in the same write as its last handshake flight, so the kernel side
    // reads both at once: the kernel must start exactly where the handshake
    // ended. For a TLS 1.3 client that flight is its Finished; for a TLS 1.2
    // server, its ChangeCipherSpec and Finished.
    let pki = pki();
    for suite in SUITES
        .into_iter()
        .filter(|s| ktls_support().supports(s.suite()))
    {
        // The kernel serves an eager client.
        let acceptor = pki.acceptor(Some(suite), Ktls::Required);
        let config = Arc::new(pki.client(Some(suite)));
        with_server(acceptor, echo, move |addr| {
            for i in 0..10u8 {
                let tcp = std::net::TcpStream::connect(addr).unwrap();
                let name = ServerName::try_from("localhost").unwrap();
                let conn = ClientConnection::new(config.clone(), name).unwrap();
                eager_peer(conn.into(), tcp, &[i; 300]);
            }
        });

        // The kernel connects to an eager server.
        let config = Arc::new(pki.server(Some(suite)));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for i in 0..10u8 {
                let (tcp, _) = listener.accept().unwrap();
                let conn = ServerConnection::new(config.clone()).unwrap();
                eager_peer(conn.into(), tcp, &[i; 300]);
            }
        });
        let connector = pki.connector(Some(suite), Ktls::Required);
        run(async move {
            for i in 0..10u8 {
                let tcp = TcpStream::connect(addr).await.unwrap();
                let tls = connector.connect("localhost", tcp).await.unwrap();
                assert!(tls.is_ktls());
                let got = recv_exactly(&tls, 300).await;
                assert_eq!(got, [i; 300], "{:?}", suite.suite());
                let (res, _) = tls.write_all(got).await;
                res.unwrap();
            }
        });
        server.join().unwrap();
    }
}

/// Drives a blocking rustls connection that queues `data` before the
/// handshake, then expects it echoed back.
fn eager_peer(mut conn: rustls::Connection, mut sock: std::net::TcpStream, data: &[u8]) {
    sock.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    // Held back until the handshake is done, then sent with the last flight
    // in one write.
    conn.writer().write_all(data).unwrap();
    loop {
        while conn.wants_write() {
            retry(|| conn.write_tls(&mut sock)).unwrap();
        }
        if !conn.is_handshaking() {
            break;
        }
        assert!(
            retry(|| conn.read_tls(&mut sock)).unwrap() > 0,
            "closed in the handshake"
        );
        conn.process_new_packets().unwrap();
    }
    let mut back = Vec::new();
    while back.len() < data.len() {
        assert!(
            retry(|| conn.read_tls(&mut sock)).unwrap() > 0,
            "closed before the echo"
        );
        let state = conn.process_new_packets().unwrap();
        while conn.wants_write() {
            retry(|| conn.write_tls(&mut sock)).unwrap();
        }
        let at = back.len();
        back.resize(at + state.plaintext_bytes_to_read(), 0);
        conn.reader().read_exact(&mut back[at..]).unwrap();
    }
    assert_eq!(back, data);
}

#[test]
fn a_blocking_rustls_peer_talks_to_a_kernel_tls_server() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    for suite in SUITES
        .into_iter()
        .filter(|s| ktls_support().supports(s.suite()))
    {
        let acceptor = pki.acceptor(Some(suite), Ktls::Required);
        let client = Arc::new(pki.client(Some(suite)));
        with_server(acceptor, echo, move |addr| {
            let mut c = blocking_client(addr, &client);
            let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
            for piece in data.chunks(300_000) {
                c.write_all(piece).unwrap();
                let mut back = vec![0; piece.len()];
                c.read_exact(&mut back).unwrap();
                assert!(back == piece);
            }
            // close_notify, answered with close_notify.
            c.conn.send_close_notify();
            retry(|| c.conn.complete_io(&mut c.sock)).unwrap();
            let mut rest = Vec::new();
            c.read_to_end(&mut rest).unwrap();
            assert!(rest.is_empty());
        });
    }
}

#[test]
fn the_kernel_never_reuses_the_nonce_of_a_tls_1_2_finished_message() {
    if !kernel_tls() {
        return;
    }
    // TLS 1.2 AES-GCM sends an 8-byte explicit nonce in every record.
    // rustls derives it as `iv XOR sequence number`, the kernel counts up
    // from what it is given: handed rustls' `iv` as it is, the kernel would
    // send its first record with the nonce of the server's Finished, under
    // the same key. Read the server's records off the wire and compare.
    let pki = pki();
    for suite in [
        suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        suites::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ] {
        if !ktls_support().supports(suite.suite()) {
            continue;
        }
        let acceptor = pki.acceptor(Some(suite), Ktls::Required);
        let config = Arc::new(pki.client(Some(suite)));
        with_server(acceptor, echo, move |addr| {
            let mut sock = std::net::TcpStream::connect(addr).unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let name = ServerName::try_from("localhost").unwrap();
            let mut conn = ClientConnection::new(config.clone(), name).unwrap();
            let mut wire = Vec::new();
            let mut read = |conn: &mut ClientConnection, sock: &mut std::net::TcpStream| {
                let mut tee = Tee {
                    sock,
                    seen: &mut wire,
                };
                assert!(retry(|| conn.read_tls(&mut tee)).unwrap() > 0, "closed");
                conn.process_new_packets().unwrap()
            };
            while conn.is_handshaking() {
                while conn.wants_write() {
                    retry(|| conn.write_tls(&mut sock)).unwrap();
                }
                if conn.is_handshaking() {
                    read(&mut conn, &mut sock);
                }
            }
            for i in 0..50u8 {
                conn.writer().write_all(&[i; 100]).unwrap();
                while conn.wants_write() {
                    retry(|| conn.write_tls(&mut sock)).unwrap();
                }
                let mut got = 0;
                while got < 100 {
                    got += read(&mut conn, &mut sock).plaintext_bytes_to_read();
                }
                let mut back = vec![0; got];
                conn.reader().read_exact(&mut back).unwrap();
                assert_eq!(back, [i; 100]);
            }
            // The server's records: plaintext handshake, ChangeCipherSpec,
            // Finished (sequence number 0, sent by rustls), then the
            // kernel's application data.
            let records = split_records(&wire);
            let ccs = records
                .iter()
                .position(|r| r.0 == 20)
                .expect("ChangeCipherSpec");
            assert_eq!(records[ccs + 1].0, 22, "Finished after ChangeCipherSpec");
            let nonces: Vec<&[u8]> = records[ccs + 1..].iter().map(|r| &r.1[..8]).collect();
            assert!(nonces.len() > 50, "{} records", nonces.len());
            let distinct: std::collections::HashSet<&[u8]> = nonces.iter().copied().collect();
            assert_eq!(
                distinct.len(),
                nonces.len(),
                "{:?}: a nonce came twice",
                suite.suite()
            );
        });
    }
}

/// Reads from `sock`, keeping a copy of every byte.
struct Tee<'a> {
    sock: &'a mut std::net::TcpStream,
    seen: &'a mut Vec<u8>,
}

impl Read for Tee<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.sock.read(buf)?;
        self.seen.extend_from_slice(&buf[..n]);
        Ok(n)
    }
}

/// TLS records: (content type, payload).
fn split_records(mut wire: &[u8]) -> Vec<(u8, &[u8])> {
    let mut records = Vec::new();
    while wire.len() >= 5 {
        let len = u16::from_be_bytes([wire[3], wire[4]]) as usize;
        records.push((wire[0], &wire[5..5 + len]));
        wire = &wire[5 + len..];
    }
    assert!(wire.is_empty(), "a record cut short");
    records
}

#[test]
fn the_peer_changing_its_keys_is_followed() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    let suites = SUITES
        .into_iter()
        .filter(|s| tls13(*s) && ktls_support().supports(s.suite()));
    for suite in suites {
        let acceptor = pki.acceptor(Some(suite), Ktls::Required);
        let client = Arc::new(pki.client(Some(suite)));
        let (tx, rx) = mpsc::channel();
        let serve = move |tls: TlsStream| {
            let tx = tx.clone();
            async move {
                let result = echo_until_error(&tls).await;
                let _ = tx.send(result);
            }
        };
        let updates = ktls_stats().key_updates;
        with_server(acceptor, serve, move |addr| {
            let mut c = blocking_client(addr, &client);
            let roundtrip = |c: &mut BlockingClient, n: u8| -> std::io::Result<()> {
                c.write_all(&[n; 1000])?;
                let mut back = [0; 1000];
                c.read_exact(&mut back)?;
                assert_eq!(back, [n; 1000]);
                Ok(())
            };
            roundtrip(&mut c, 1).unwrap();
            // A KeyUpdate asking for one back, several times: each moves
            // both directions to new keys.
            for n in 2..6 {
                c.conn.refresh_traffic_keys().unwrap();
                let res = roundtrip(&mut c, n);
                if !ktls_support().key_update() {
                    assert!(res.is_err(), "kept going without new keys");
                    break;
                }
                res.unwrap();
            }
            drop(c);
            // What the server saw, before the server is shut down.
            let ended = rx.recv_timeout(Duration::from_secs(20)).unwrap();
            if ktls_support().key_update() {
                assert!(ended.is_ok(), "{ended:?}");
                // Each KeyUpdate: the peer's key, and ours in answer.
                assert!(ktls_stats().key_updates >= updates + 8);
            } else {
                let err = ended.unwrap_err();
                assert_eq!(code(&err), Some(Code::KtlsKeyUpdate), "{err}");
            }
        });
    }
}

#[test]
fn a_kernel_tls_client_follows_a_server_changing_keys() {
    if !kernel_tls() || !kernel_key_updates() {
        return;
    }
    let pki = pki();
    let server = Arc::new(pki.server(Some(suites::TLS13_AES_256_GCM_SHA384)));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // A blocking rustls server: echoes, changing keys before every answer.
    let server = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let conn = ServerConnection::new(server).unwrap();
        let mut s = StreamOwned::new(conn, tcp);
        let mut buf = [0; 100];
        for _ in 0..5 {
            s.read_exact(&mut buf).unwrap();
            s.conn.refresh_traffic_keys().unwrap();
            s.write_all(&buf).unwrap();
        }
    });
    let connector = pki.connector(Some(suites::TLS13_AES_256_GCM_SHA384), Ktls::Required);
    run(async move {
        let tcp = TcpStream::connect(addr).await.unwrap();
        let tls = connector.connect("localhost", tcp).await.unwrap();
        assert!(tls.is_ktls());
        for n in 0..5u8 {
            let (res, _) = tls.write_all(vec![n; 100]).await;
            res.unwrap();
            assert_eq!(recv_exactly(&tls, 100).await, vec![n; 100]);
        }
    });
    server.join().unwrap();
}

#[test]
fn close_notify_is_told_apart_from_a_cut_connection() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    let (tx, rx) = mpsc::channel();
    let serve = move |tls: TlsStream| {
        let tx = tx.clone();
        async move {
            let end = loop {
                match tls.recv().await {
                    Ok(data) if data.is_empty() => break Ok(tls.received_close_notify()),
                    Ok(_) => {}
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send(end.map_err(|e| e.to_string()));
        }
    };
    let acceptor = pki.acceptor(Some(suites::TLS13_AES_128_GCM_SHA256), Ktls::Required);
    let client = Arc::new(pki.client(None));
    with_server(acceptor, serve, move |addr| {
        let wait = Duration::from_secs(20);
        let mut c = blocking_client(addr, &client);
        c.write_all(b"bye").unwrap();
        c.conn.send_close_notify();
        retry(|| c.conn.complete_io(&mut c.sock)).unwrap();
        assert_eq!(rx.recv_timeout(wait).unwrap(), Ok(true));

        let mut c = blocking_client(addr, &client);
        c.write_all(b"cut").unwrap();
        // A FIN without close_notify. (Not Shutdown::Both: a TLS 1.3 server
        // may still be sending its session tickets, and data arriving at a
        // socket shut for reading makes the kernel reset the connection.)
        c.sock.shutdown(std::net::Shutdown::Write).unwrap();
        assert_eq!(rx.recv_timeout(wait).unwrap(), Ok(false));
    });
}

#[test]
fn a_tampered_record_is_an_error_not_data() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    for suite in [
        suites::TLS13_AES_128_GCM_SHA256,
        suites::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    ] {
        let (tx, rx) = mpsc::channel();
        let serve = move |tls: TlsStream| {
            let tx = tx.clone();
            async move {
                // The server's handshake ends when the client's Finished
                // arrives. Say so before the client sends the tampered
                // record: arriving with the Finished, it would be rustls,
                // still finishing the handshake, that rejects it.
                if tls.write_all(b"ready".to_vec()).await.0.is_ok() {
                    let _ = tx.send(tls.recv().await.map(|d| d.len()));
                }
            }
        };
        let acceptor = pki.acceptor(Some(suite), Ktls::Required);
        let client = Arc::new(pki.client(Some(suite)));
        with_server(acceptor, serve, move |addr| {
            let mut c = blocking_client(addr, &client);
            let mut ready = [0; 5];
            c.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
            // Encrypt, then flip a bit of the authentication tag.
            c.conn.writer().write_all(b"pay 100 to alice").unwrap();
            let mut record = Vec::new();
            c.conn.write_tls(&mut record).unwrap();
            *record.last_mut().unwrap() ^= 1;
            c.sock.write_all(&record).unwrap();
            let got = rx.recv_timeout(Duration::from_secs(20)).unwrap();
            let err = got.expect_err("a tampered record must not be data");
            assert_eq!(code(&err), Some(Code::TlsProtocol), "{err}");
            assert!(err.to_string().contains("bad_record_mac"), "{err}");
        });
    }
}

/// A control message setting the type of the record `sendmsg` sends.
fn record_type(record_type: u8) -> Vec<u8> {
    let space = unsafe { libc::CMSG_SPACE(1) } as usize;
    let mut buf = vec![0u64; space.div_ceil(8)];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_control = buf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `buf` is aligned and holds one header and one byte of data.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = 282; // SOL_TLS
        (*cmsg).cmsg_type = 1; // TLS_SET_RECORD_TYPE
        (*cmsg).cmsg_len = libc::CMSG_LEN(1) as _;
        *libc::CMSG_DATA(cmsg) = record_type;
    }
    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), space) }.to_vec()
}

#[test]
fn records_that_are_not_data_are_handled_as_rustls_would() {
    if !kernel_tls() {
        return;
    }
    const CCS: u8 = 20;
    const ALERT: u8 = 21;
    const HANDSHAKE: u8 = 22;
    // (suite, records the client sends, whether the server goes on, what
    // its error says if not)
    let tls12 = suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256;
    let tls13 = suites::TLS13_AES_128_GCM_SHA256;
    type Case = (
        SupportedCipherSuite,
        Vec<(u8, Vec<u8>)>,
        Result<(), &'static str>,
    );
    let cases: Vec<Case> = vec![
        (tls13, vec![(CCS, vec![1])], Err("change_cipher_spec")),
        (
            tls13,
            vec![(HANDSHAKE, vec![99, 0, 0, 0])],
            Err("handshake message 99"),
        ),
        (tls13, vec![(ALERT, vec![1, 90])], Ok(())), // warning user_canceled
        (tls13, vec![(ALERT, vec![1, 10])], Err("UnexpectedMessage")),
        (tls13, vec![(ALERT, vec![2, 40])], Err("HandshakeFailure")),
        (tls13, vec![(ALERT, vec![1, 0, 0])], Err("alert of 3 bytes")),
        (tls13, vec![(ALERT, vec![9, 40])], Err("alert of level 9")),
        (tls13, vec![(24, vec![1])], Err("unknown type 24")),
        (tls12, vec![(ALERT, vec![1, 90]); 4], Ok(())),
        (
            tls12,
            vec![(ALERT, vec![1, 90]); 5],
            Err("too many warning alerts"),
        ),
        // A renegotiation request is refused once, then fatal.
        (tls12, vec![(HANDSHAKE, vec![1, 0, 0, 0])], Ok(())),
        (
            tls12,
            vec![(HANDSHAKE, vec![1, 0, 0, 0]); 2],
            Err("renegotiate again"),
        ),
        (
            tls12,
            vec![(HANDSHAKE, vec![4, 0, 0, 0])],
            Err("handshake message 4"),
        ),
    ];
    let pki = pki();
    for (suite, records, expected) in cases {
        let acceptor = pki.acceptor(Some(suite), Ktls::Required);
        let connector = pki.connector(Some(suite), Ktls::Required);
        let label = format!("{:?} {records:?}", suite.suite());
        run(async move {
            let (server, client) = tls_pair(acceptor, connector).await;
            for (typ, body) in records {
                let (res, _, _) = client.tcp().send_with_control(body, record_type(typ)).await;
                res.unwrap();
            }
            let (res, _) = client.write_all(b"after".to_vec()).await;
            res.unwrap();
            let got = timeout(Duration::from_secs(20), server.recv())
                .await
                .unwrap();
            match expected {
                Ok(()) => assert_eq!(got.unwrap(), b"after", "{label}"),
                Err(what) => {
                    let err = got.expect_err(&label);
                    assert!(err.to_string().contains(what), "{label}: {err}");
                    // And so does every later receive.
                    let again = server.recv().await.expect_err(&label);
                    assert!(again.to_string().contains(what), "{label}: {again}");
                    drop(server);
                    // The server's fatal alert, if it sent one, reaches the
                    // client as an error, never as a clean end.
                    let end = timeout(Duration::from_secs(20), client.recv())
                        .await
                        .unwrap();
                    if let Ok(data) = end {
                        assert!(
                            data.is_empty() && !client.received_close_notify(),
                            "{label}"
                        );
                    }
                }
            }
        });
    }
}

#[test]
fn a_stream_already_read_with_recv_stays_in_the_process_under_auto() {
    if !kernel_tls() {
        return;
    }
    // A protocol with a plaintext greeting before TLS, read with `recv`:
    // the runtime may hold bytes after the greeting, so the kernel cannot
    // take the stream over.
    let pki = pki();
    let suite = suites::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256;
    for mode in [Ktls::Auto, Ktls::Required] {
        let acceptor = pki.acceptor(Some(suite), Ktls::Off);
        let connector = pki.connector(Some(suite), mode);
        let before = ktls_stats();
        run(async move {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = giong::spawn(async move {
                let tcp = listener.accept().await.unwrap();
                let (res, _) = tcp.write_all(b"HELLO\n".to_vec()).await;
                res.unwrap();
                let tls = acceptor.accept(tcp).await?;
                echo(tls).await;
                Ok::<_, std::io::Error>(())
            });
            let tcp = TcpStream::connect(addr).await.unwrap();
            let greeting = tcp.recv().await.unwrap();
            assert_eq!(&greeting[..], b"HELLO\n");
            drop(greeting);
            match (mode, connector.connect("localhost", tcp).await) {
                (Ktls::Auto, Ok(tls)) => {
                    assert!(!tls.is_ktls());
                    let (res, _) = tls.write_all(b"ping".to_vec()).await;
                    res.unwrap();
                    assert_eq!(recv_exactly(&tls, 4).await, b"ping");
                    tls.shutdown().await.unwrap();
                    server.await.unwrap().unwrap();
                }
                (Ktls::Required, Err(e)) => {
                    assert_eq!(code(&e), Some(Code::KtlsUnavailable), "{e}");
                    assert!(e.to_string().contains("recv"), "{e}");
                    assert!(server.await.unwrap().is_err());
                }
                (mode, other) => panic!("{mode:?}: {other:?}"),
            }
        });
        if mode == Ktls::Auto {
            assert!(ktls_stats().stayed_in_process > before.stayed_in_process);
        }
    }
}

#[test]
fn kernel_recv_is_cancel_safe() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    let acceptor = pki.acceptor(Some(suites::TLS13_AES_128_GCM_SHA256), Ktls::Required);
    let connector = pki.connector(Some(suites::TLS13_AES_128_GCM_SHA256), Ktls::Required);
    run(async move {
        let (server, client) = tls_pair(acceptor, connector).await;
        let sender = giong::spawn(async move {
            for i in 0..200u32 {
                let (res, _) = client.write_all(i.to_be_bytes().to_vec()).await;
                res.unwrap();
                if i % 10 == 0 {
                    sleep(Duration::from_millis(5)).await;
                }
            }
            client.shutdown().await.unwrap();
            client
        });
        let mut got = Vec::new();
        let mut cancelled = 0;
        loop {
            match timeout(Duration::from_millis(1), server.recv()).await {
                Err(_) => cancelled += 1,
                Ok(data) => {
                    let data = data.unwrap();
                    if data.is_empty() {
                        break;
                    }
                    got.extend_from_slice(&data);
                }
            }
        }
        let expected: Vec<u8> = (0..200u32).flat_map(u32::to_be_bytes).collect();
        assert!(got == expected, "{} of {} bytes", got.len(), expected.len());
        assert!(cancelled > 0, "no receive was cancelled");
        assert!(server.received_close_notify());
        drop(sender.await.unwrap());
        drop(server);
        sleep(Duration::from_millis(50)).await;
        let stats = giong::stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn a_cancelled_kernel_write_ends_sending_instead_of_cutting_a_message() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    for vectored in [false, true] {
        let acceptor = pki.acceptor(Some(suites::TLS13_AES_128_GCM_SHA256), Ktls::Required);
        let connector = pki.connector(Some(suites::TLS13_AES_128_GCM_SHA256), Ktls::Required);
        run(async move {
            let (server, client) = tls_pair(acceptor, connector).await;
            // Nobody reads on the client, so a big write fills the buffers and
            // is cancelled half way.
            let big = vec![7u8; 64 << 20];
            let cancelled = if vectored {
                let write = server.write_all_vectored(vec![vec![7u8; 100], big]);
                timeout(Duration::from_millis(200), write).await.is_err()
            } else {
                timeout(Duration::from_millis(200), server.write_all(big))
                    .await
                    .is_err()
            };
            assert!(cancelled, "the write finished; make it bigger");
            let (res, _) = server.write_all(b"more".to_vec()).await;
            let err = res.unwrap_err();
            assert_eq!(code(&err), Some(Code::TlsBroken), "{err}");
            // What did arrive is intact: sevens, then the end.
            let mut total = 0;
            while let Ok(Ok(data)) = timeout(Duration::from_millis(500), client.recv()).await {
                if data.is_empty() {
                    break;
                }
                assert!(data.iter().all(|&b| b == 7));
                total += data.len();
            }
            assert!(total > 0);
            drop((server, client));
            sleep(Duration::from_millis(50)).await;
            let stats = giong::stats();
            assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        });
    }
}

#[test]
fn one_task_may_receive_while_another_sends() {
    if !kernel_tls() {
        return;
    }
    let pki = pki();
    let acceptor = pki.acceptor(Some(suites::TLS13_AES_256_GCM_SHA384), Ktls::Required);
    let connector = pki.connector(None, Ktls::Off);
    run(async move {
        let (server, client) = tls_pair(acceptor, connector).await;
        let server = std::rc::Rc::new(server);
        let writer = {
            let server = server.clone();
            giong::spawn(async move {
                for i in 0..100u8 {
                    let (res, _) = server.write_all(vec![i; 10_000]).await;
                    res.unwrap();
                }
            })
        };
        let echo_back = giong::spawn(async move {
            let got = recv_exactly(&client, 1_000_000).await;
            let (res, _) = client.write_all(got).await;
            res.unwrap();
            client
        });
        let back = recv_exactly(&server, 1_000_000).await;
        let expected: Vec<u8> = (0..100u8).flat_map(|i| vec![i; 10_000]).collect();
        assert!(back == expected);
        writer.await.unwrap();
        drop(echo_back.await.unwrap());
    });
}

// ------------------------------------------------------- blocking peers

/// `f` again while it fails with `Interrupted`. A blocking socket call with
/// a timeout fails that way when the thread is interrupted, as io_uring does
/// to run work for a thread that had a ring; rustls' `read_tls` and
/// `write_tls` do not retry.
fn retry<T>(mut f: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    loop {
        match f() {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

type BlockingClient = StreamOwned<ClientConnection, std::net::TcpStream>;

fn blocking_client(addr: SocketAddr, config: &Arc<ClientConfig>) -> BlockingClient {
    let tcp = std::net::TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    StreamOwned::new(ClientConnection::new(config.clone(), name).unwrap(), tcp)
}

/// Runs `serve` for every connection `acceptor` accepts, on a runtime of
/// its own, until `client` returns.
fn with_server<F, Fut>(acceptor: TlsAcceptor, serve: F, client: impl FnOnce(SocketAddr))
where
    F: Fn(TlsStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + 'static,
{
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let (tx, rx) = mpsc::channel();
    let serve = Arc::new(serve);
    let server = std::thread::spawn(move || {
        rt.run(move |_| {
            let (tx, acceptor, serve) = (tx.clone(), acceptor.clone(), serve.clone());
            async move {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                while let Some(Ok(tcp)) = giong::until_shutdown(listener.accept()).await {
                    let (acceptor, serve) = (acceptor.clone(), serve.clone());
                    giong::spawn(async move {
                        match acceptor.accept(tcp).await {
                            Ok(tls) => serve(tls).await,
                            Err(e) => eprintln!("handshake failed: {e}"),
                        }
                    });
                }
            }
        })
        .unwrap();
    });
    client(rx.recv().unwrap());
    handle.shutdown();
    server.join().unwrap();
}

async fn echo(tls: TlsStream) {
    let _ = echo_until_error(&tls).await;
    let _ = tls.shutdown().await;
}

/// Echoes until the peer closes (`Ok`) or something fails.
async fn echo_until_error(tls: &TlsStream) -> std::io::Result<()> {
    loop {
        let data = tls.recv().await?;
        if data.is_empty() {
            return Ok(());
        }
        tls.write_all(data).await.0?;
    }
}
