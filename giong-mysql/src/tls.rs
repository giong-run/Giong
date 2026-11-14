//! `ssl-mode=VERIFY_CA`: the server's certificate is checked against the
//! given authorities, but not against the host name.
//!
//! MySQL generates a certificate authority and a server certificate when it
//! is installed. The server certificate names no host
//! (`MySQL_Server_<version>_Auto_Generated_Server_Certificate`), so a check
//! of the host name always fails, and only a check of the chain can trust
//! it. That is safe only with authorities that sign for this server alone,
//! such as the server's own `ca.pem`: with public authorities, any
//! certificate they signed, for any name, would pass.

use std::fmt;
use std::sync::Arc;

use giong_tls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use giong_tls::rustls::client::WebPkiServerVerifier;
use giong_tls::rustls::crypto::{ring, CryptoProvider};
use giong_tls::rustls::pki_types::pem::PemObject;
use giong_tls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use giong_tls::rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore,
    SignatureScheme,
};
use giong_tls::TlsConnector;

use crate::error::Error;

/// Checks the chain like rustls' own verifier, and accepts a certificate
/// whose only fault is that it does not name the host.
struct ChainOnly(Arc<WebPkiServerVerifier>);

impl fmt::Debug for ChainOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ChainOnly")
    }
}

impl ServerCertVerifier for ChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            // rustls checks the name only once the chain, the validity
            // period and the key usage have passed (the test
            // `the_chain_is_still_checked` makes sure of it).
            Err(TlsError::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// A connector that trusts the authorities in `pem` and does not check the
/// host name.
pub(crate) fn chain_only(pem: &[u8]) -> Result<TlsConnector, Error> {
    let (verifier, provider) = verifier(pem)?;
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::config(format!("cannot set up TLS: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(TlsConnector::new(Arc::new(config)))
}

fn verifier(pem: &[u8]) -> Result<(ChainOnly, Arc<CryptoProvider>), Error> {
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(pem) {
        let cert = cert.map_err(|e| {
            Error::config(format!(
                "the certificate authority PEM cannot be read ({e}); give the server's ca.pem"
            ))
        })?;
        roots.add(cert).map_err(|e| {
            Error::config(format!(
                "a certificate in the authority PEM is not usable ({e}); give the server's ca.pem"
            ))
        })?;
    }
    if roots.is_empty() {
        return Err(Error::config(
            "the certificate authority PEM holds no certificate; give the server's ca.pem",
        ));
    }
    let provider = Arc::new(ring::default_provider());
    let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| {
            Error::config(format!(
                "cannot check certificates with these authorities: {e}"
            ))
        })?;
    Ok((ChainOnly(webpki), provider))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn authority(name: &str) -> (rcgen::Certificate, rcgen::KeyPair) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        (params.self_signed(&key).unwrap(), key)
    }

    fn server_cert(
        ca: &rcgen::Certificate,
        ca_key: &rcgen::KeyPair,
        names: &[&str],
        expired: bool,
    ) -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().unwrap();
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        let mut params = rcgen::CertificateParams::new(names).unwrap();
        // As MySQL names its own: in the common name only.
        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            "MySQL_Server_8.0.46_Auto_Generated_Server_Certificate",
        );
        if expired {
            params.not_before = rcgen::date_time_ymd(2020, 1, 1);
            params.not_after = rcgen::date_time_ymd(2021, 1, 1);
        }
        let cert = params.signed_by(&key, ca, ca_key).unwrap();
        CertificateDer::from(cert.der().to_vec())
    }

    #[test]
    fn the_chain_is_still_checked() {
        let (ca, ca_key) = authority("ours");
        let (verifier, _) = verifier(ca.pem().as_bytes()).unwrap();
        let name = ServerName::try_from("db.internal").unwrap();
        let check = |cert: &CertificateDer<'_>| {
            verifier.verify_server_cert(cert, &[], &name, &[], UnixTime::now())
        };
        // Signed by the authority: accepted, whatever names it has.
        for names in [&[][..], &["localhost"], &["db.internal"]] {
            check(&server_cert(&ca, &ca_key, names, false)).unwrap();
        }
        // Expired: refused.
        let e = check(&server_cert(&ca, &ca_key, &[], true)).unwrap_err();
        assert!(
            matches!(
                e,
                TlsError::InvalidCertificate(
                    CertificateError::Expired | CertificateError::ExpiredContext { .. }
                )
            ),
            "{e:?}"
        );
        // Another authority's, or self-signed: refused, even with the
        // right name.
        let (other, other_key) = authority("theirs");
        for cert in [
            server_cert(&other, &other_key, &["db.internal"], false),
            CertificateDer::from(other.der().to_vec()),
        ] {
            let e = check(&cert).unwrap_err();
            assert!(matches!(e, TlsError::InvalidCertificate(_)), "{e:?}");
        }
        // The authority itself, offered as the server's certificate.
        assert!(check(&CertificateDer::from(ca.der().to_vec())).is_err());
        // PEM that holds no certificate.
        assert!(verifier_from(b"").is_err());
        assert!(
            verifier_from(b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n")
                .is_err()
        );
    }

    fn verifier_from(pem: &[u8]) -> Result<(), Error> {
        verifier(pem).map(drop)
    }
}
