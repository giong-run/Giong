//! The server side through the stable API alone: certificates chosen by SNI,
//! replaced while the server runs, and client certificates.

use std::cell::RefCell;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use giong::error::{code, Code};
use giong::net::{TcpListener, TcpStream};
use giong_tls::{Certificates, TlsAcceptor, TlsConnector, TlsStream};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose,
};

/// A certificate authority that issues server and client certificates.
struct Authority {
    cert: rcgen::Certificate,
    key: KeyPair,
}

impl Authority {
    fn new(name: &str) -> Self {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let cert = params.self_signed(&key).unwrap();
        Self { cert, key }
    }

    fn pem(&self) -> String {
        self.cert.pem()
    }

    /// A certificate and its key, for `names` (a server) or for none (a
    /// client).
    fn issue(&self, names: &[&str]) -> (String, String) {
        let key = KeyPair::generate().unwrap();
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let server = !names.is_empty();
        let mut params = CertificateParams::new(names).unwrap();
        params.extended_key_usages = vec![if server {
            ExtendedKeyUsagePurpose::ServerAuth
        } else {
            params
                .distinguished_name
                .push(DnType::CommonName, "giong test client");
            ExtendedKeyUsagePurpose::ClientAuth
        }];
        let cert = params.signed_by(&key, &self.cert, &self.key).unwrap();
        (cert.pem(), key.serialize_pem())
    }
}

/// Accepts connections on a new port and sends the outcome of each
/// handshake to `outcomes`; a connection that succeeds answers "ok".
fn serve(acceptor: TlsAcceptor, outcomes: Rc<RefCell<Vec<io::Result<()>>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    giong::spawn(async move {
        while let Ok(tcp) = listener.accept().await {
            let acceptor = acceptor.clone();
            let outcomes = outcomes.clone();
            giong::spawn(async move {
                match acceptor.accept(tcp).await {
                    Ok(tls) => {
                        outcomes.borrow_mut().push(Ok(()));
                        let _ = tls.write_all(b"ok".to_vec()).await;
                        let _ = tls.shutdown().await;
                    }
                    Err(e) => outcomes.borrow_mut().push(Err(e)),
                }
            });
        }
    });
    addr
}

/// Connects, reads the server's answer, and returns the stream.
async fn talk(connector: &TlsConnector, addr: SocketAddr, name: &str) -> io::Result<TlsStream> {
    let tcp = TcpStream::connect(addr).await?;
    let tls = connector.connect(name, tcp).await?;
    let data = tls.recv().await?;
    assert_eq!(data, b"ok");
    Ok(tls)
}

fn first_cert(pem: &str) -> Vec<u8> {
    use rustls_pki_types::pem::PemObject;
    rustls_pki_types::CertificateDer::from_pem_slice(pem.as_bytes())
        .unwrap()
        .to_vec()
}

#[test]
fn the_name_the_client_asks_for_picks_the_certificate() {
    let ca = Authority::new("giong test CA");
    let (api, api_key) = ca.issue(&["api.localhost"]);
    let (www, www_key) = ca.issue(&["www.localhost"]);
    let certs = Certificates::from_pem(api.as_bytes(), api_key.as_bytes()).unwrap();
    certs.add_pem(www.as_bytes(), www_key.as_bytes()).unwrap();
    let acceptor = TlsAcceptor::with_certificates(&certs).unwrap();
    let connector = TlsConnector::with_root_pem(ca.pem().as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, Rc::default());
        for (name, pem) in [("api.localhost", &api), ("www.localhost", &www)] {
            let tls = talk(&connector, addr, name).await.unwrap();
            let presented = tls.peer_certificates().unwrap();
            assert_eq!(presented[0].to_vec(), first_cert(pem), "{name}");
        }
        // No certificate for this name: the first one is presented, and the
        // client refuses it.
        let err = talk(&connector, addr, "db.localhost").await.unwrap_err();
        assert_eq!(code(&err), Some(Code::TlsCertificate), "{err}");
    });
}

#[test]
fn replaced_certificates_serve_the_next_handshake() {
    let old_ca = Authority::new("old CA");
    let new_ca = Authority::new("new CA");
    let (old, old_key) = old_ca.issue(&["localhost"]);
    let (new, new_key) = new_ca.issue(&["localhost"]);
    let certs = Certificates::from_pem(old.as_bytes(), old_key.as_bytes()).unwrap();
    let acceptor = TlsAcceptor::with_certificates(&certs).unwrap();
    let trusts_old = TlsConnector::with_root_pem(old_ca.pem().as_bytes()).unwrap();
    let trusts_new = TlsConnector::with_root_pem(new_ca.pem().as_bytes()).unwrap();
    giong::block_on(async move {
        let addr = serve(acceptor, Rc::default());
        let open = talk(&trusts_old, addr, "localhost").await.unwrap();
        certs
            .replace_pem(&[(new.as_bytes(), new_key.as_bytes())])
            .unwrap();
        talk(&trusts_new, addr, "localhost").await.unwrap();
        // A client that resumes a session skips the certificate, as TLS
        // allows; one without a session sees the new certificate.
        let fresh = TlsConnector::with_root_pem(old_ca.pem().as_bytes()).unwrap();
        let err = talk(&fresh, addr, "localhost").await.unwrap_err();
        assert_eq!(code(&err), Some(Code::TlsCertificate), "{err}");
        // The connection opened before keeps its certificate.
        assert_eq!(
            open.peer_certificates().unwrap()[0].to_vec(),
            first_cert(&old)
        );
    });
}

#[test]
fn a_required_client_certificate_must_come_from_the_authority() {
    let ca = Authority::new("giong test CA");
    let stranger = Authority::new("some other CA");
    let (server, server_key) = ca.issue(&["localhost"]);
    let (client, client_key) = ca.issue(&[]);
    let (other, other_key) = stranger.issue(&[]);
    let acceptor = TlsAcceptor::with_certificates(
        &Certificates::from_pem(server.as_bytes(), server_key.as_bytes()).unwrap(),
    )
    .unwrap()
    .alpn(&[b"h2"])
    .require_client_cert(ca.pem().as_bytes())
    .unwrap();
    let plain = TlsConnector::with_root_pem(ca.pem().as_bytes())
        .unwrap()
        .alpn(&[b"h2"]);
    let with_cert = plain
        .clone()
        .client_auth(client.as_bytes(), client_key.as_bytes())
        .unwrap();
    let with_other = plain
        .clone()
        .client_auth(other.as_bytes(), other_key.as_bytes())
        .unwrap();
    giong::block_on(async move {
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let addr = serve(acceptor, outcomes.clone());
        let tls = talk(&with_cert, addr, "localhost").await.unwrap();
        // ALPN set before require_client_cert still holds.
        assert_eq!(tls.alpn_protocol().as_deref(), Some(&b"h2"[..]));
        assert!(talk(&plain, addr, "localhost").await.is_err());
        assert!(talk(&with_other, addr, "localhost").await.is_err());
        let outcomes = outcomes.borrow();
        assert_eq!(outcomes.len(), 3);
        assert!(outcomes[0].is_ok());
        for e in [&outcomes[1], &outcomes[2]] {
            let e = e.as_ref().unwrap_err();
            assert!(
                matches!(code(e), Some(Code::TlsCertificate | Code::TlsProtocol)),
                "{e}"
            );
        }
    });
}

#[test]
fn a_requested_client_certificate_may_be_missing() {
    let ca = Authority::new("giong test CA");
    let (server, server_key) = ca.issue(&["localhost"]);
    let (client, client_key) = ca.issue(&[]);
    let acceptor = TlsAcceptor::with_certificates(
        &Certificates::from_pem(server.as_bytes(), server_key.as_bytes()).unwrap(),
    )
    .unwrap()
    .request_client_cert(ca.pem().as_bytes())
    .unwrap();
    let plain = TlsConnector::with_root_pem(ca.pem().as_bytes()).unwrap();
    let with_cert = plain
        .clone()
        .client_auth(client.as_bytes(), client_key.as_bytes())
        .unwrap();
    giong::block_on(async move {
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let addr = serve(acceptor, outcomes.clone());
        talk(&plain, addr, "localhost").await.unwrap();
        talk(&with_cert, addr, "localhost").await.unwrap();
        assert!(outcomes.borrow().iter().all(Result::is_ok));
    });
}

#[test]
fn client_certificates_need_an_acceptor_giong_built() {
    use giong_tls::rustls;
    use rustls::pki_types::pem::PemObject;
    let ca = Authority::new("giong test CA");
    let (server, server_key) = ca.issue(&["localhost"]);
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from_pem_slice(server.as_bytes()).unwrap()],
            rustls::pki_types::PrivateKeyDer::from_pem_slice(server_key.as_bytes()).unwrap(),
        )
        .unwrap();
    let err = TlsAcceptor::from_rustls(std::sync::Arc::new(config))
        .require_client_cert(ca.pem().as_bytes())
        .unwrap_err();
    assert_eq!(code(&err), Some(Code::TlsConfig), "{err}");
}
