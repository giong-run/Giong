//! What two writers and a used-up key do to `write_all`, in the process and
//! under kernel TLS.

use std::rc::Rc;
use std::sync::Arc;

use giong::error::Code;
use giong::net::{TcpListener, TcpStream};
use giong_tls::rustls::crypto::CipherSuiteCommon;
use giong_tls::rustls::{self, ClientConfig, ServerConfig, SupportedCipherSuite, Tls12CipherSuite};
use giong_tls::{Ktls, TlsAcceptor, TlsConnector, TlsStream};

/// TLS 1.2 ECDHE-ECDSA AES-128-GCM that may encrypt only `limit` records
/// under one key instead of 2^24, so that a test reaches the limit.
fn tls12_with_record_limit(limit: u64) -> SupportedCipherSuite {
    let SupportedCipherSuite::Tls12(suite) =
        rustls::crypto::ring::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
    else {
        unreachable!("a TLS 1.2 suite")
    };
    let limited: &'static Tls12CipherSuite = Box::leak(Box::new(Tls12CipherSuite {
        common: CipherSuiteCommon {
            confidentiality_limit: limit,
            ..suite.common
        },
        ..*suite
    }));
    SupportedCipherSuite::Tls12(limited)
}

fn configs(suite: SupportedCipherSuite) -> (Arc<ServerConfig>, Arc<ClientConfig>) {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let mut provider = rustls::crypto::ring::default_provider();
    provider.cipher_suites = vec![suite];
    let provider = Arc::new(provider);
    let der = rustls::pki_types::PrivatePkcs8KeyDer::from(key.key_pair.serialize_der());
    let server = ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[suite.version()])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![key.cert.der().clone()], der.into())
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(key.cert.der().clone()).unwrap();
    let client = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[suite.version()])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    (Arc::new(server), Arc::new(client))
}

async fn pair(acceptor: TlsAcceptor, connector: TlsConnector) -> (TlsStream, TlsStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let accept = giong::spawn(async move {
        let tcp = listener.accept().await.unwrap();
        acceptor.accept(tcp).await.unwrap()
    });
    let tcp = TcpStream::connect(addr).await.unwrap();
    let client = connector.connect("localhost", tcp).await.unwrap();
    (accept.await.unwrap(), client)
}

/// Everything the peer sends until the end of the stream, whether it ended
/// with `close_notify`, and the error that ended it, if one did.
async fn drain(tls: TlsStream) -> (Vec<u8>, bool, Option<String>) {
    let mut got = Vec::new();
    loop {
        match tls.recv().await {
            Ok(data) if data.is_empty() => return (got, tls.received_close_notify(), None),
            Ok(data) => got.extend_from_slice(&data),
            Err(e) => return (got, tls.received_close_notify(), Some(e.to_string())),
        }
    }
}

/// rustls cannot change TLS 1.2 keys: once a key has encrypted as many
/// records as its cipher allows, rustls sends `close_notify` and drops what
/// it is given. `write_all` used to report that data as sent.
#[test]
fn a_tls12_write_past_the_record_limit_fails_instead_of_vanishing() {
    let (server, client) = configs(tls12_with_record_limit(40));
    let (acceptor, connector) = (TlsAcceptor::new(server), TlsConnector::new(client));
    giong::block_on(async move {
        let (server, client) = pair(acceptor, connector).await;
        assert!(!server.is_ktls());
        let reader = giong::spawn(drain(client));
        let mut sent = 0;
        let mut first_error = None;
        for i in 0..100u8 {
            match server.write_all(vec![i; 1000]).await.0 {
                Ok(()) => sent += 1000,
                Err(e) => {
                    first_error = Some(e);
                    break;
                }
            }
        }
        let e = first_error.expect("every write succeeded past the record limit");
        assert_eq!(giong::error::code(&e), Some(Code::TlsKeysExhausted), "{e}");
        // And every later one, the vectored and file writes included.
        let (res, _) = server.write_all(b"more".to_vec()).await;
        let e = res.unwrap_err();
        assert_eq!(giong::error::code(&e), Some(Code::TlsKeysExhausted), "{e}");
        let (res, _) = server.write_all_vectored(vec![b"more".to_vec()]).await;
        assert_eq!(
            giong::error::code(&res.unwrap_err()),
            Some(Code::TlsKeysExhausted)
        );
        server.shutdown().await.unwrap();

        let (got, close_notify, err) = reader.await.unwrap();
        assert_eq!(err, None);
        assert!(close_notify, "the peer saw no close_notify");
        assert_eq!(
            got.len(),
            sent,
            "the peer did not get what was reported sent"
        );
        assert!(got
            .chunks(1000)
            .enumerate()
            .all(|(i, c)| c.iter().all(|&b| b == i as u8)));
    });
}

/// Two tasks call `write_all` with 1 MiB each on one stream: each write
/// arrives whole, one after the other.
fn two_writers(ktls: Ktls) {
    let suite = rustls::crypto::ring::cipher_suite::TLS13_AES_128_GCM_SHA256;
    let (server, client) = configs(suite);
    let acceptor = TlsAcceptor::new(server).ktls(ktls).unwrap();
    let connector = TlsConnector::new(client);
    giong::block_on(async move {
        let (server, client) = pair(acceptor, connector).await;
        assert_eq!(server.is_ktls(), ktls == Ktls::Required);
        let server = Rc::new(server);
        let reader = giong::spawn(drain(client));
        const LEN: usize = 1 << 20;
        let mut writers = Vec::new();
        for (byte, vectored) in [(b'A', false), (b'B', true), (b'C', false)] {
            let server = server.clone();
            writers.push(giong::spawn(async move {
                if vectored {
                    let half = vec![byte; LEN / 2];
                    server.write_all_vectored(vec![half.clone(), half]).await.0
                } else {
                    server.write_all(vec![byte; LEN]).await.0
                }
            }));
        }
        for writer in writers {
            writer.await.unwrap().unwrap();
        }
        server.shutdown().await.unwrap();
        let (got, _, err) = reader.await.unwrap();
        assert_eq!(err, None);
        assert_eq!(got.len(), 3 * LEN);
        let runs = 1 + got.windows(2).filter(|w| w[0] != w[1]).count();
        assert_eq!(runs, 3, "concurrent writes interleaved their data");
    });
}

#[test]
fn concurrent_writes_do_not_interleave_in_the_process() {
    two_writers(Ktls::Off);
}

#[test]
fn concurrent_writes_do_not_interleave_under_kernel_tls() {
    if !giong_tls::ktls_support().is_available() {
        assert!(
            std::env::var_os("GIONG_KTLS_REQUIRED").is_none(),
            "{}",
            giong_tls::ktls_support()
        );
        eprintln!("skipped: {}", giong_tls::ktls_support());
        return;
    }
    two_writers(Ktls::Required);
}
