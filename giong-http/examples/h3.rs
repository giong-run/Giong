//! HTTP/3 with giong-http: `Server::serve_h3` on a giong-quic endpoint.
//! Experimental, behind the `unstable-http3` feature.
//!
//!     cargo run --release -p giong-http --features unstable-http3 --example h3 -- 127.0.0.1:8443
//!     curl -k --http3-only https://localhost:8443/
//!     h3spec localhost 8443
//!
//! Without `GIONG_CERT` and `GIONG_KEY` (PEM files), a self-signed
//! certificate for `localhost` is made at start. One worker serves the
//! port, which keeps the example short; a server can run an endpoint on
//! every worker, on the same port.
//!
//! `GIONG_EARLY_DATA=1` takes 0-RTT requests from resuming clients
//! (h3spec's 0-RTT case needs it). `GIONG_QPACK_TABLE=<bytes>` allows
//! clients a QPACK dynamic table.
//!
//! Routes: `/` (a small page), `/bytes?n=<len>`, `/echo` (the request body
//! back), `/stream?n=<pieces>` (16 KiB pieces, produced as they are sent).

use std::sync::Arc;
use std::time::Duration;

use giong_http::{Body, Request, Response, Server};
use giong_quic::{Endpoint, ServerConfig};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

fn query_number(req: &Request, key: &str) -> Option<usize> {
    req.query()?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        if k == key {
            v.parse().ok()
        } else {
            None
        }
    })
}

async fn handle(req: Request) -> Response {
    match req.path() {
        "/" => Response::new(200)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(format!("Hello from Giong over {}\n", req.version())),
        "/bytes" => {
            let n = query_number(&req, "n").unwrap_or(1024).min(64 << 20);
            Response::new(200)
                .header("Content-Type", "application/octet-stream")
                .body(vec![b'x'; n])
        }
        "/echo" => Response::new(200).body(req.body().to_vec()),
        "/stream" => {
            let pieces = query_number(&req, "n").unwrap_or(16).min(65536);
            let (tx, body) = Body::channel();
            drop(giong::spawn(async move {
                for _ in 0..pieces {
                    if tx.send(vec![b'y'; 16 * 1024]).await.is_err() {
                        return;
                    }
                }
            }));
            Response::new(200)
                .header("Content-Type", "application/octet-stream")
                .body(body)
        }
        _ => Response::new(404).body("not found\n"),
    }
}

/// The certificate chain and key: from `GIONG_CERT` and `GIONG_KEY`, or
/// self-signed for `localhost`.
fn identity() -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
    if let (Ok(cert), Ok(key)) = (std::env::var("GIONG_CERT"), std::env::var("GIONG_KEY")) {
        let chain = CertificateDer::pem_file_iter(cert)
            .expect("GIONG_CERT")
            .collect::<Result<Vec<_>, _>>()
            .expect("GIONG_CERT");
        return (chain, PrivateKeyDer::from_pem_file(key).expect("GIONG_KEY"));
    }
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(made.key_pair.serialize_der());
    (vec![made.cert.der().clone()], key.into())
}

fn main() {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8443".into());
    let (chain, key) = identity();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("TLS 1.3")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("certificate and key");
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let early = std::env::var("GIONG_EARLY_DATA").is_ok_and(|v| v == "1");
    let config = ServerConfig::new(tls)
        .expect("a QUIC server configuration")
        .early_data(early);

    let rt = giong::Builder::new()
        .workers(1)
        // QUIC receives coalesced datagrams (GRO).
        .recv_buffers(256, giong::net::GRO_RECV_BUFFER)
        .shutdown_grace(Duration::from_secs(5))
        .build()
        .expect("runtime");
    println!("https://{addr}  (HTTP/3 over QUIC, UDP)");
    rt.run(move |_| {
        let (addr, config) = (addr.clone(), config.clone());
        async move {
            let endpoint =
                Endpoint::server(addr.parse().expect("an address"), config).expect("bind");
            let qpack = std::env::var("GIONG_QPACK_TABLE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            Server::new()
                .max_body_size(16 << 20)
                .qpack_table_size(qpack)
                .serve_h3(endpoint, handle)
                .await;
        }
    })
    .expect("run");
}
