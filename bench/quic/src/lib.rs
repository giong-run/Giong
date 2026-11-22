//! Shared pieces of the QUIC decision benchmark.
//!
//! Wire protocol of every server: the client opens a bidirectional stream,
//! writes the response size as a big-endian `u64` and finishes its side; the
//! server answers with that many bytes and finishes. `rr` asks for small
//! responses, `bulk` for large ones.

use std::sync::Arc;

pub mod engine;

pub const ALPN: &[u8] = b"giong-bench";

/// A self-signed certificate for `localhost`, made at start-up.
pub fn certificate() -> (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).expect("rcgen");
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    (vec![cert.cert.der().clone()], key.into())
}

/// The transport settings every server uses, so that only the I/O layer
/// differs. Windows are raised from quinn's defaults so that one connection
/// is not limited by flow control on loopback.
pub fn transport() -> quinn_proto::TransportConfig {
    let mut t = quinn_proto::TransportConfig::default();
    t.max_concurrent_bidi_streams(1024u32.into())
        .stream_receive_window((8u32 << 20).into())
        .receive_window((32u32 << 20).into())
        .send_window(32 << 20);
    t
}

pub fn server_config() -> quinn_proto::ServerConfig {
    let (certs, key) = certificate();
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls.max_early_data_size = 0;
    let crypto = quinn_proto::crypto::rustls::QuicServerConfig::try_from(tls).unwrap();
    let mut cfg = quinn_proto::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport()));
    cfg
}

/// The client accepts any certificate: this is a benchmark on loopback.
pub fn client_config() -> quinn::ClientConfig {
    #[derive(Debug)]
    struct AnyCert(Arc<rustls::crypto::CryptoProvider>);
    impl rustls::client::danger::ServerCertVerifier for AnyCert {
        fn verify_server_cert(
            &self,
            _: &rustls::pki_types::CertificateDer<'_>,
            _: &[rustls::pki_types::CertificateDer<'_>],
            _: &rustls::pki_types::ServerName<'_>,
            _: &[u8],
            _: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                m,
                c,
                d,
                &self.0.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &rustls::pki_types::CertificateDer<'_>,
            d: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                m,
                c,
                d,
                &self.0.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport()));
    cfg
}

/// User plus system CPU time of a process, in clock ticks, from /proc; on
/// another machine through `ssh <host> cat /proc/<pid>/stat` when `ssh` is
/// given.
pub fn cpu_ticks_on(ssh: Option<&str>, pid: u32) -> u64 {
    let stat = match ssh {
        None => std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default(),
        Some(host) => std::process::Command::new("ssh")
            .args([host, "cat", &format!("/proc/{pid}/stat")])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default(),
    };
    parse_ticks(&stat)
}

/// User plus system CPU time of a local process, in clock ticks.
pub fn cpu_ticks(pid: u32) -> u64 {
    cpu_ticks_on(None, pid)
}

fn parse_ticks(stat: &str) -> u64 {
    // Fields after the command name, which is in parentheses.
    let rest = stat.rsplit_once(')').map(|(_, r)| r).unwrap_or("");
    let f: Vec<&str> = rest.split_whitespace().collect();
    // utime and stime are fields 14 and 15 of the whole line: 12 and 13 here.
    f.get(11).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
        + f.get(12).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)
}
