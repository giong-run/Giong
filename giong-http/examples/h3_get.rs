//! The HTTP/3 client of giong-http: GETs a URL over QUIC, for trying it
//! against other servers. Experimental, behind the `unstable-http3`
//! feature.
//!
//!     cargo run --release -p giong-http --features unstable-http3,client \
//!         --example h3_get -- https://localhost:8443/ 10
//!
//! The second argument is how many requests to send, one after another on
//! one connection (default 1). `GIONG_CA=<file>` (PEM, required) holds
//! the certificates trusted to sign the server's; no others are.
//! `GIONG_QPACK_TABLE=<bytes>` allows the server a QPACK dynamic table for
//! its responses.

use std::sync::Arc;
use std::time::Instant;

use giong_http::client::Client;
use giong_quic::ClientConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::CertificateDer;

fn main() {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("a URL");
    let n: usize = args.next().map_or(1, |n| n.parse().expect("a count"));
    let ca = std::env::var("GIONG_CA").expect("GIONG_CA, a PEM certificate to trust");
    let table = std::env::var("GIONG_QPACK_TABLE").map_or(0, |v| v.parse().expect("bytes"));
    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(ca).expect("GIONG_CA") {
        roots.add(cert.expect("GIONG_CA")).expect("GIONG_CA");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"h3".to_vec()];
    let client = Client::builder()
        .http3_prior_knowledge(ClientConfig::new(tls).unwrap())
        .qpack_table_size(table)
        .build();
    giong::Builder::new()
        .block_on(async move {
            let start = Instant::now();
            let mut bytes = 0;
            for i in 0..n {
                let res = client.get(&url).send().await.unwrap();
                bytes += res.body().len();
                if i == 0 {
                    println!("{} {:?}", res.status(), res.version());
                    for (name, value) in res.headers() {
                        println!("{name}: {}", String::from_utf8_lossy(value));
                    }
                }
            }
            println!("{n} requests, {bytes} body bytes, {:?}", start.elapsed());
        })
        .unwrap();
}
