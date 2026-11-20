use axum::{body::Bytes, extract::Query, routing::{get, post}, Json, Router};
use serde::{Deserialize, Serialize};
use std::hint::black_box;

#[derive(Serialize)]
struct Msg { message: &'static str }

#[derive(Serialize)]
struct User { id: u32, name: String, email: String, active: bool, score: f64 }

#[derive(Deserialize)]
struct N { n: Option<u32> }

fn fib(n: u32) -> u64 { if n < 2 { n as u64 } else { fib(n - 1) + fib(n - 2) } }

async fn json() -> Json<Msg> { Json(Msg { message: "Hello, World!" }) }

async fn users() -> Json<Vec<User>> {
    Json((0..100).map(|i| User {
        id: i, name: format!("User {i}"), email: format!("user{i}@example.com"),
        active: i % 2 == 0, score: i as f64 * 1.5,
    }).collect())
}

async fn compute(Query(q): Query<N>) -> Json<serde_json::Value> {
    let n = black_box(q.n.unwrap_or(25));
    Json(serde_json::json!({ "result": fib(n) }))
}

/// `POST /upload` reads the whole body before answering (bench/small-box.sh).
async fn upload(body: Bytes) -> String { body.len().to_string() }

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/json", get(json))
        .route("/users", get(users))
        .route("/compute", get(compute))
        .route("/upload", post(upload));
    // HTTPS with the certificate named by TLS_CERT / TLS_KEY (PEM files).
    rustls::crypto::ring::default_provider().install_default().unwrap();
    let cert = std::env::var("TLS_CERT").expect("TLS_CERT");
    let key = std::env::var("TLS_KEY").expect("TLS_KEY");
    // PREFER_AES128=1 (bench/tls.sh, axum-tls-aes128): pick AES-128-GCM first
    // whatever order the client lists, as giong-tls' `TlsAcceptor::with_certificates`
    // does, to show how much of a handshake that choice is.
    let tls = if std::env::var_os("PREFER_AES128").is_some() {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use rustls::CipherSuite as S;
        let mut provider = rustls::crypto::ring::default_provider();
        provider.cipher_suites.sort_by_key(|s| {
            let rank = match s.suite() {
                S::TLS13_AES_128_GCM_SHA256
                | S::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
                | S::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => 0,
                S::TLS13_CHACHA20_POLY1305_SHA256
                | S::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
                | S::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => 1,
                _ => 2,
            };
            (s.version().version != rustls::ProtocolVersion::TLSv1_3, rank)
        });
        let certs = CertificateDer::pem_file_iter(&cert).unwrap().map(Result::unwrap).collect();
        let key = PrivateKeyDer::from_pem_file(&key).unwrap();
        let mut config = rustls::ServerConfig::builder_with_provider(std::sync::Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        config.ignore_client_order = true;
        // What `RustlsConfig::from_pem_file` sets.
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec(), b"http/1.0".to_vec()];
        axum_server::tls_rustls::RustlsConfig::from_config(std::sync::Arc::new(config))
    } else {
        axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await.unwrap()
    };
    // BENCH_HOST (default 127.0.0.1): the address to listen on (bench/small-box.sh).
    let host = std::env::var("BENCH_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let server = axum_server::bind_rustls(format!("{host}:3011").parse().unwrap(), tls);
    // TCP_NODELAY=1 (bench/h2.sh): set it on each connection, as giong does.
    if std::env::var_os("TCP_NODELAY").is_some() {
        let server = server.map(|tls| tls.acceptor(axum_server::accept::NoDelayAcceptor::new()));
        server.serve(app.into_make_service()).await.unwrap();
    } else {
        server.serve(app.into_make_service()).await.unwrap();
    }
}
