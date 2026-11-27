//! What the HTTP/3 servers and the client share: the routes' bodies, the address, the
//! certificate and a latency histogram.

use std::net::SocketAddr;
use std::sync::Arc;

use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// `GET /json`, as everywhere in bench/.
pub const JSON: &[u8] = br#"{"message":"Hello, World!"}"#;

/// `GET /users`: the 100 users of bench/giong-http (8.3 KB), built once.
pub fn users() -> &'static [u8] {
    use std::io::Write;
    static USERS: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    USERS.get_or_init(|| {
        let mut body = Vec::with_capacity(8500);
        body.push(b'[');
        for i in 0..100u32 {
            if i > 0 {
                body.push(b',');
            }
            let _ = write!(
                body,
                r#"{{"id":{i},"name":"User {i}","email":"user{i}@example.com","active":{},"score":{}}}"#,
                i % 2 == 0,
                i as f64 * 1.5
            );
        }
        body.push(b']');
        body
    })
}

/// `BENCH_HOST` (default 127.0.0.1), UDP port 3012.
pub fn addr() -> SocketAddr {
    let host = std::env::var("BENCH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    format!("{host}:3012").parse().expect("BENCH_HOST")
}

pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// TLS 1.3 with ALPN `h3`, from the PEM files in `TLS_CERT` and `TLS_KEY`.
pub fn server_tls() -> rustls::ServerConfig {
    let cert = std::env::var("TLS_CERT").expect("TLS_CERT");
    let key = std::env::var("TLS_KEY").expect("TLS_KEY");
    let chain = CertificateDer::pem_file_iter(cert)
        .expect("TLS_CERT")
        .collect::<Result<Vec<_>, _>>()
        .expect("TLS_CERT");
    let key = PrivateKeyDer::from_pem_file(key).expect("TLS_KEY");
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("certificate and key");
    tls.alpn_protocols = vec![b"h3".to_vec()];
    tls
}

/// Latencies in microseconds: 1 us steps up to 10 ms, 100 us steps up to 1 s.
pub struct Hist {
    fine: Vec<u64>,
    coarse: Vec<u64>,
    over: u64,
    max: u64,
    n: u64,
}

impl Default for Hist {
    fn default() -> Self {
        Hist { fine: vec![0; 10_000], coarse: vec![0; 10_000], over: 0, max: 0, n: 0 }
    }
}

impl Hist {
    pub fn add(&mut self, us: u64) {
        self.n += 1;
        self.max = self.max.max(us);
        if us < 10_000 {
            self.fine[us as usize] += 1;
        } else if us < 1_000_000 {
            self.coarse[(us / 100) as usize] += 1;
        } else {
            self.over += 1;
        }
    }

    pub fn merge(&mut self, o: &Hist) {
        for (a, b) in self.fine.iter_mut().zip(&o.fine) {
            *a += b;
        }
        for (a, b) in self.coarse.iter_mut().zip(&o.coarse) {
            *a += b;
        }
        self.over += o.over;
        self.max = self.max.max(o.max);
        self.n += o.n;
    }

    pub fn count(&self) -> u64 {
        self.n
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    pub fn quantile(&self, q: f64) -> u64 {
        let want = ((self.n as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (us, c) in self.fine.iter().enumerate() {
            seen += c;
            if seen >= want {
                return us as u64;
            }
        }
        for (i, c) in self.coarse.iter().enumerate() {
            seen += c;
            if seen >= want {
                return i as u64 * 100;
            }
        }
        self.max
    }
}
