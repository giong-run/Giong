//! The certificate authorities Windows trusts for TLS servers, read from the
//! system's certificate stores.
//!
//! Windows keeps trusted roots in the `ROOT` store. Opened for the current
//! user, it holds the user's own roots and, inherited, the machine's: the
//! ones Microsoft ships, the ones Windows Update added and the ones an
//! administrator or a group policy installed. A root that is also in the
//! `Disallowed` store (the same certificate, byte for byte) is left out, as
//! Windows itself does. An intermediate there is not refused: rustls takes
//! the roots to trust, not certificates to refuse along a chain.
//!
//! Of the roots, only those valid for TLS server authentication are kept: a
//! certificate restricted to other uses (code signing, e-mail) through its
//! extended key usage, or through the usages the administrator set on it,
//! does not vouch for servers.
//!
//! Windows downloads some of Microsoft's roots the first time its own
//! verifier needs them, so a fresh machine may hold fewer than the full
//! program. A server whose root is missing fails to verify with
//! `UnknownIssuer`; `SSL_CERT_FILE` or `TlsConnector::with_webpki_roots`
//! cover that case.

use std::collections::HashSet;
use std::ptr;

use rustls::pki_types::CertificateDer;
use windows_sys::Win32::Foundation::{GetLastError, CRYPT_E_NOT_FOUND};
use windows_sys::Win32::Security::Cryptography::{
    CertCloseStore, CertEnumCertificatesInStore, CertGetEnhancedKeyUsage, CertOpenSystemStoreW,
    CERT_CONTEXT, CTL_USAGE, HCERTSTORE,
};

/// The OID of TLS server authentication, as the usage list spells it.
const SERVER_AUTH: &[u8] = b"1.3.6.1.5.5.7.3.1";

/// An open system store, closed when dropped.
struct Store(HCERTSTORE);

impl Store {
    /// The current user's view of the system store `name`, if it opens.
    fn open(name: &str) -> Option<Self> {
        let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 string alive for the
        // call; no provider handle is passed.
        let store = unsafe { CertOpenSystemStoreW(0, wide.as_ptr()) };
        (!store.is_null()).then_some(Store(store))
    }

    /// The DER of every certificate in the store, with whether it is valid
    /// for TLS server authentication.
    fn certificates(&self) -> Vec<(Vec<u8>, bool)> {
        let mut out = Vec::new();
        let mut cert: *const CERT_CONTEXT = ptr::null();
        loop {
            // SAFETY: `cert` is null or the context the previous call
            // returned, which this call frees; the store is open.
            cert = unsafe { CertEnumCertificatesInStore(self.0, cert) };
            if cert.is_null() {
                return out;
            }
            // SAFETY: a non-null context from the store, alive until the
            // next call; its encoded bytes are valid for their length.
            let der = unsafe {
                let c = &*cert;
                if c.pbCertEncoded.is_null() || c.cbCertEncoded == 0 {
                    continue;
                }
                std::slice::from_raw_parts(c.pbCertEncoded, c.cbCertEncoded as usize).to_vec()
            };
            out.push((der, server_auth(cert)));
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // SAFETY: the store is open and closed once; no context from it is
        // still held (enumeration frees each one).
        unsafe { CertCloseStore(self.0, 0) };
    }
}

/// Whether `cert` may vouch for TLS servers: no usage restriction at all,
/// or one that lists server authentication.
fn server_auth(cert: *const CERT_CONTEXT) -> bool {
    let mut len = 0u32;
    // SAFETY: a valid context; the first call only reports the size.
    if unsafe { CertGetEnhancedKeyUsage(cert, 0, ptr::null_mut(), &mut len) } == 0 {
        return false;
    }
    // Aligned for `CTL_USAGE` and the pointers that follow it.
    let mut buf = vec![0u64; (len as usize).div_ceil(8).max(1)];
    let usage = buf.as_mut_ptr().cast::<CTL_USAGE>();
    // SAFETY: `buf` holds `len` writable, aligned bytes.
    if unsafe { CertGetEnhancedKeyUsage(cert, 0, usage, &mut len) } == 0 {
        return false;
    }
    // SAFETY: the call filled a `CTL_USAGE` at the start of `buf`, whose
    // identifiers point into `buf` as NUL-terminated strings.
    unsafe {
        let usage = &*usage;
        if usage.cUsageIdentifier == 0 {
            // No list: every use, unless the call says the certificate has
            // been restricted to none.
            return GetLastError() as i32 == CRYPT_E_NOT_FOUND;
        }
        (0..usage.cUsageIdentifier as usize).any(|i| {
            let oid = *usage.rgpszUsageIdentifier.add(i);
            !oid.is_null() && std::ffi::CStr::from_ptr(oid.cast()).to_bytes() == SERVER_AUTH
        })
    }
}

/// The certificates Windows trusts as roots for TLS servers, in DER. Empty
/// when the store cannot be read.
pub(crate) fn trusted_roots() -> Vec<CertificateDer<'static>> {
    let Some(root) = Store::open("ROOT") else {
        return Vec::new();
    };
    let disallowed: HashSet<Vec<u8>> = Store::open("Disallowed")
        .map(|s| s.certificates().into_iter().map(|(der, _)| der).collect())
        .unwrap_or_default();
    let mut seen = HashSet::new();
    root.certificates()
        .into_iter()
        .filter(|(der, server)| *server && !disallowed.contains(der) && seen.insert(der.clone()))
        .map(|(der, _)| CertificateDer::from(der))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_store_has_roots_and_rustls_can_use_them() {
        let roots = trusted_roots();
        assert!(!roots.is_empty(), "no trusted roots in the ROOT store");
        let mut store = rustls::RootCertStore::empty();
        let (added, _) = store.add_parsable_certificates(roots);
        assert!(added > 0);
    }
}
