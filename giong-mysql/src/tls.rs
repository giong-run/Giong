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

use giong_tls::TlsConnector;

use crate::error::Error;

/// A connector that trusts the authorities in `pem` and does not check the
/// host name ([`TlsConnector::with_root_pem_any_name`], whose tests check
/// that the chain still is).
pub(crate) fn chain_only(pem: &[u8]) -> Result<TlsConnector, Error> {
    TlsConnector::with_root_pem_any_name(pem).map_err(|e| {
        Error::config(format!(
            "the certificate authority PEM cannot be used ({e}); give the server's ca.pem"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_authority_pem_must_hold_a_certificate() {
        for pem in [
            &b""[..],
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
        ] {
            let e = chain_only(pem).unwrap_err().to_string();
            assert!(e.contains("give the server's ca.pem"), "{e}");
        }
    }
}
