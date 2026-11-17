//! The server's certificates: one or several, chosen by the name the client
//! asks for (SNI), and replaceable while the server runs.

use std::fmt;
use std::io;
use std::sync::{Arc, RwLock};

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;

use crate::{bad_certificate_data, bad_key, mismatched_key, pem_certs, provider};

/// The certificates a [`TlsAcceptor`](crate::TlsAcceptor) presents: one
/// chain or several, and replaceable while the server runs.
///
/// Cheap to clone; clones share the same set. With several, each handshake
/// gets the first one valid for the name the client asked for through SNI,
/// and the first one of all when the client sent no name or none matches.
/// [`replace_pem`](Self::replace_pem) swaps the whole set at once, for a
/// renewed certificate: handshakes that start afterwards get the new set,
/// connections already open keep theirs. A client that resumes an earlier
/// session is not shown a certificate again, as TLS allows: session tickets
/// stay valid across a replacement.
///
/// ```no_run
/// use giong_tls::{Certificates, TlsAcceptor};
///
/// # fn main() -> std::io::Result<()> {
/// let certs = Certificates::from_pem(&std::fs::read("api.pem")?, &std::fs::read("api.key")?)?;
/// certs.add_pem(&std::fs::read("www.pem")?, &std::fs::read("www.key")?)?;
/// let acceptor = TlsAcceptor::with_certificates(&certs)?;
///
/// // Later, when the certificates have been renewed:
/// certs.replace_pem(&[
///     (&std::fs::read("api.pem")?, &std::fs::read("api.key")?),
///     (&std::fs::read("www.pem")?, &std::fs::read("www.key")?),
/// ])?;
/// # let _ = acceptor;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Certificates {
    set: Arc<RwLock<Arc<[Arc<CertifiedKey>]>>>,
}

impl Certificates {
    /// One certificate chain and its private key, both in PEM form, the
    /// server's own certificate first.
    pub fn from_pem(cert_chain: &[u8], private_key: &[u8]) -> io::Result<Self> {
        let key = certified(cert_chain, private_key)?;
        Ok(Self {
            set: Arc::new(RwLock::new(Arc::from(vec![key]))),
        })
    }

    /// Adds another chain and its key, presented to clients that ask for a
    /// name it is valid for. Every clone, and every acceptor made from one,
    /// sees it from the next handshake on.
    pub fn add_pem(&self, cert_chain: &[u8], private_key: &[u8]) -> io::Result<()> {
        let key = certified(cert_chain, private_key)?;
        let mut set = self.set.write().unwrap_or_else(|e| e.into_inner());
        let mut keys = set.to_vec();
        keys.push(key);
        *set = Arc::from(keys);
        Ok(())
    }

    /// Replaces the whole set with `chains`, each a certificate chain and its
    /// private key in PEM form, in the order of preference. Every chain is
    /// checked first: on an error nothing changes.
    pub fn replace_pem(&self, chains: &[(&[u8], &[u8])]) -> io::Result<()> {
        if chains.is_empty() {
            return Err(bad_certificate_data(
                "replace_pem was given no certificate; a server needs at least one",
            ));
        }
        let keys = chains
            .iter()
            .map(|(chain, key)| certified(chain, key))
            .collect::<io::Result<Vec<_>>>()?;
        *self.set.write().unwrap_or_else(|e| e.into_inner()) = Arc::from(keys);
        Ok(())
    }

    /// How many chains the set holds.
    pub fn len(&self) -> usize {
        self.current().len()
    }

    /// `false`: a set always holds at least one chain.
    pub fn is_empty(&self) -> bool {
        self.current().is_empty()
    }

    fn current(&self) -> Arc<[Arc<CertifiedKey>]> {
        self.set.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn resolver(&self) -> Arc<dyn ResolvesServerCert> {
        Arc::new(Resolver(self.clone()))
    }
}

impl fmt::Debug for Certificates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificates")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

fn certified(cert_chain: &[u8], private_key: &[u8]) -> io::Result<Arc<CertifiedKey>> {
    use rustls::pki_types::pem::PemObject;
    let certs: Vec<CertificateDer<'static>> = pem_certs(cert_chain)?;
    let key = PrivateKeyDer::from_pem_slice(private_key).map_err(bad_key)?;
    let key = CertifiedKey::from_der(certs, key, &provider()).map_err(mismatched_key)?;
    Ok(Arc::new(key))
}

/// Picks the certificate for a ClientHello.
struct Resolver(Certificates);

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Resolver")
    }
}

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let set = self.0.current();
        if set.len() > 1 {
            if let Some(name) = hello.server_name() {
                if let Some(key) = for_name(&set, name) {
                    return Some(key);
                }
            }
        }
        set.first().cloned()
    }
}

/// The first certificate of `set` valid for the DNS name `name`.
fn for_name(set: &[Arc<CertifiedKey>], name: &str) -> Option<Arc<CertifiedKey>> {
    let name = rustls::pki_types::ServerName::try_from(name).ok()?;
    set.iter()
        .find(|key| {
            key.end_entity_cert()
                .ok()
                .and_then(|der| webpki::EndEntityCert::try_from(der).ok())
                .is_some_and(|cert| cert.verify_is_valid_for_subject_name(&name).is_ok())
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(names: &[&str]) -> (String, String) {
        let key = rcgen::KeyPair::generate().unwrap();
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let cert = rcgen::CertificateParams::new(names)
            .unwrap()
            .self_signed(&key)
            .unwrap();
        (cert.pem(), key.serialize_pem())
    }

    fn chosen(certs: &Certificates, name: &str) -> Vec<u8> {
        let set = certs.current();
        for_name(&set, name)
            .or_else(|| set.first().cloned())
            .unwrap()
            .end_entity_cert()
            .unwrap()
            .to_vec()
    }

    fn der(pem: &str) -> Vec<u8> {
        pem_certs(pem.as_bytes()).unwrap()[0].to_vec()
    }

    #[test]
    fn the_name_picks_the_certificate_and_the_first_is_the_fallback() {
        let (api, api_key) = pair(&["api.example.com"]);
        let (www, www_key) = pair(&["www.example.com", "example.com"]);
        let (wild, wild_key) = pair(&["*.internal.example.com"]);
        let certs = Certificates::from_pem(api.as_bytes(), api_key.as_bytes()).unwrap();
        certs.add_pem(www.as_bytes(), www_key.as_bytes()).unwrap();
        certs.add_pem(wild.as_bytes(), wild_key.as_bytes()).unwrap();
        assert_eq!(certs.len(), 3);
        assert_eq!(chosen(&certs, "api.example.com"), der(&api));
        assert_eq!(chosen(&certs, "example.com"), der(&www));
        assert_eq!(chosen(&certs, "db.internal.example.com"), der(&wild));
        assert_eq!(chosen(&certs, "other.org"), der(&api));
    }

    #[test]
    fn replace_is_all_or_nothing() {
        let (a, a_key) = pair(&["a.example.com"]);
        let (b, b_key) = pair(&["b.example.com"]);
        let certs = Certificates::from_pem(a.as_bytes(), a_key.as_bytes()).unwrap();
        let clone = certs.clone();
        // A key that does not belong to its certificate: nothing changes.
        let err = certs
            .replace_pem(&[
                (b.as_bytes(), b_key.as_bytes()),
                (b.as_bytes(), a_key.as_bytes()),
            ])
            .unwrap_err();
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::BadCertificateData),
            "{err}"
        );
        assert_eq!(chosen(&clone, "b.example.com"), der(&a));
        assert!(certs.replace_pem(&[]).is_err());
        certs
            .replace_pem(&[(b.as_bytes(), b_key.as_bytes())])
            .unwrap();
        assert_eq!(clone.len(), 1);
        assert_eq!(chosen(&clone, "a.example.com"), der(&b));
    }
}
