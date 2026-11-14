//! What the tests against real crates share: certificates and connected
//! streams.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong_tls::rustls::pki_types::pem::PemObject;
use giong_tls::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use giong_tls::rustls::{self, ClientConfig, RootCertStore, ServerConfig};
use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};

/// Generous: nothing here should take more than a fraction of it.
pub const LIMIT: Duration = Duration::from_secs(30);

/// A certificate authority and a server certificate it issued for
/// `localhost`, all PEM.
pub struct Pki {
    pub ca: String,
    pub cert: String,
    pub key: String,
}

pub fn pki() -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca.distinguished_name
        .push(DnType::CommonName, "giong-compat test CA");
    ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    let cert = params.signed_by(&key, &ca_cert, &ca_key).unwrap();
    Pki {
        ca: ca_cert.pem(),
        cert: cert.pem(),
        key: key.serialize_pem(),
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A rustls server configuration for `pki`, offering `alpn`.
pub fn server_config(pki: &Pki, alpn: &[&[u8]]) -> Arc<ServerConfig> {
    let certs = vec![CertificateDer::from_pem_slice(pki.cert.as_bytes()).unwrap()];
    let key = PrivateKeyDer::from_pem_slice(pki.key.as_bytes()).unwrap();
    let mut config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

/// A rustls client configuration trusting `pki`'s CA, asking for `alpn`.
pub fn client_config(pki: &Pki, alpn: &[&[u8]]) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(pki.ca.as_bytes()).unwrap())
        .unwrap();
    let mut config = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Arc::new(config)
}

pub fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
}

/// A connected pair of giong streams: (client, server).
pub async fn pair() -> (TcpStream, TcpStream) {
    let (l, addr) = listener();
    let c = TcpStream::connect(addr).await.unwrap();
    let s = l.accept().await.unwrap();
    (c, s)
}

pub fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 + i / 251) as u8).collect()
}
