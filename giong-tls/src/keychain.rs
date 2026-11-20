//! The certificate authorities macOS trusts for TLS servers, read from the
//! keychain's trust settings.
//!
//! macOS keeps three sets of trust settings: the system's (the root
//! certificates Apple ships), the administrator's and the user's. A
//! certificate listed in one of them is trusted as a root unless its
//! settings say otherwise, and the user's settings win over the
//! administrator's, which win over the system's: that is how a company's own
//! authority gets added, and how a user turns a shipped root off.
//!
//! This reads all three and returns the certificates trusted for the TLS
//! server policy, leaving out the ones that are denied. It is what
//! `Security.framework` documents for `SecTrustSettingsCopyTrustSettings`,
//! reduced to the question "is this certificate a trusted root for a TLS
//! server": a setting that holds for one host name or one application only
//! is passed over, so such a certificate is not trusted for every server,
//! and the errors a setting allows (an expired certificate, a wrong name)
//! are never allowed here.

use std::ffi::c_void;
use std::ptr;

use rustls::pki_types::CertificateDer;

type CFTypeRef = *const c_void;
type CFIndex = isize;
type OSStatus = i32;

/// `SecTrustSettingsDomain`: user, administrator, system. In the order
/// they are read: the first settings found for a certificate win.
const DOMAINS: [u32; 3] = [0, 1, 2];

/// `SecTrustSettingsResult`.
const RESULT_INVALID: i64 = 0;
const RESULT_TRUST_ROOT: i64 = 1;
const RESULT_TRUST_AS_ROOT: i64 = 2;
const RESULT_UNSPECIFIED: i64 = 4;

/// `kCFStringEncodingUTF8`.
const UTF8: u32 = 0x0800_0100;
/// `kCFNumberSInt64Type`.
const SINT64: CFIndex = 4;

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(cf: CFTypeRef);
    fn CFEqual(a: CFTypeRef, b: CFTypeRef) -> u8;
    fn CFGetTypeID(cf: CFTypeRef) -> usize;
    fn CFArrayGetTypeID() -> usize;
    fn CFArrayGetCount(array: CFTypeRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFTypeRef, index: CFIndex) -> CFTypeRef;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFDataGetLength(data: CFTypeRef) -> CFIndex;
    fn CFDictionaryGetTypeID() -> usize;
    fn CFDictionaryGetValue(dict: CFTypeRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetTypeID() -> usize;
    fn CFNumberGetValue(number: CFTypeRef, kind: CFIndex, value: *mut c_void) -> u8;
    fn CFStringCreateWithBytes(
        allocator: CFTypeRef,
        bytes: *const u8,
        len: CFIndex,
        encoding: u32,
        external: u8,
    ) -> CFTypeRef;
}

#[link(name = "Security", kind = "framework")]
extern "C" {
    static kSecPolicyOid: CFTypeRef;
    static kSecPolicyAppleSSL: CFTypeRef;
    fn SecTrustSettingsCopyCertificates(domain: u32, certs: *mut CFTypeRef) -> OSStatus;
    fn SecTrustSettingsCopyTrustSettings(
        cert: CFTypeRef,
        domain: u32,
        settings: *mut CFTypeRef,
    ) -> OSStatus;
    fn SecCertificateCopyData(cert: CFTypeRef) -> CFTypeRef;
    fn SecPolicyGetTypeID() -> usize;
    fn SecPolicyCopyProperties(policy: CFTypeRef) -> CFTypeRef;
}

/// An object this code owns one reference to, released when dropped.
struct Owned(CFTypeRef);

impl Owned {
    /// Takes over the reference a `Copy` or `Create` function returned.
    /// `None` for null.
    fn new(cf: CFTypeRef) -> Option<Self> {
        (!cf.is_null()).then_some(Self(cf))
    }

    fn string(text: &str) -> Option<Self> {
        // SAFETY: `text` is valid for its length; the null allocator is the
        // default one. The function copies the bytes.
        Self::new(unsafe {
            CFStringCreateWithBytes(ptr::null(), text.as_ptr(), text.len() as CFIndex, UTF8, 0)
        })
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a non-null object we hold one reference to.
        unsafe { CFRelease(self.0) };
    }
}

/// The elements of `array`, which stay alive as long as it does. Empty when
/// `array` is not an array.
fn elements(array: &Owned) -> impl Iterator<Item = CFTypeRef> + '_ {
    // SAFETY: `array` is a live object; the count is asked of an array only.
    let count = unsafe {
        if CFGetTypeID(array.0) == CFArrayGetTypeID() {
            CFArrayGetCount(array.0)
        } else {
            0
        }
    };
    // SAFETY: the index is within the array, which nothing changes.
    (0..count).map(move |i| unsafe { CFArrayGetValueAtIndex(array.0, i) })
}

/// What one set of trust settings says about a certificate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trust {
    /// Trusted as a root for TLS servers.
    Root,
    /// Denied.
    No,
}

/// The names of the keys in a trust settings dictionary. They are macros in
/// the C headers, not symbols, so the strings are made here.
struct Keys {
    policy: Owned,
    result: Owned,
    /// Constraints that narrow a setting to one host name or application.
    narrowing: [Owned; 2],
}

impl Keys {
    fn new() -> Option<Self> {
        Some(Self {
            policy: Owned::string("kSecTrustSettingsPolicy")?,
            result: Owned::string("kSecTrustSettingsResult")?,
            narrowing: [
                Owned::string("kSecTrustSettingsPolicyString")?,
                Owned::string("kSecTrustSettingsApplication")?,
            ],
        })
    }
}

/// Whether `policy` (a `SecPolicyRef`) is the TLS policy.
fn is_tls_policy(policy: CFTypeRef) -> bool {
    // SAFETY: `policy` is a live object from a settings dictionary; it is
    // only passed on as a policy once its type says it is one. The
    // properties are a dictionary we own; the two constants are immutable
    // strings of the framework.
    unsafe {
        if CFGetTypeID(policy) != SecPolicyGetTypeID() {
            return false;
        }
        let Some(properties) = Owned::new(SecPolicyCopyProperties(policy)) else {
            return false;
        };
        let oid = CFDictionaryGetValue(properties.0, kSecPolicyOid);
        !oid.is_null() && CFEqual(oid, kSecPolicyAppleSSL) != 0
    }
}

/// What `settings`, the trust settings array of one certificate in one
/// domain, says about TLS servers. `None` when it says nothing about them
/// (the next domain is asked then).
fn evaluate(settings: &Owned, keys: &Keys) -> Option<Trust> {
    // "An empty trust settings array means always trust this certificate,
    // with a result of kSecTrustSettingsResultTrustRoot."
    let mut any = false;
    for entry in elements(settings) {
        any = true;
        // SAFETY: `entry` is alive while `settings` is; it is only read as a
        // dictionary once its type says it is one, and its values likewise.
        unsafe {
            if entry.is_null() || CFGetTypeID(entry) != CFDictionaryGetTypeID() {
                continue;
            }
            // A setting for another policy (S/MIME, code signing) says
            // nothing about TLS; one without a policy is for all of them.
            let policy = CFDictionaryGetValue(entry, keys.policy.0);
            if !policy.is_null() && !is_tls_policy(policy) {
                continue;
            }
            // A setting for one host name or one application is not a
            // statement about every server.
            if keys
                .narrowing
                .iter()
                .any(|key| !CFDictionaryGetValue(entry, key.0).is_null())
            {
                continue;
            }
            // Without a result the setting means "trust as a root".
            let number = CFDictionaryGetValue(entry, keys.result.0);
            let mut result = RESULT_TRUST_ROOT;
            if !number.is_null() {
                let read = CFGetTypeID(number) == CFNumberGetTypeID()
                    && CFNumberGetValue(number, SINT64, ptr::addr_of_mut!(result).cast()) != 0;
                if !read {
                    continue;
                }
            }
            match result {
                // No statement: the next entry, then the next domain.
                RESULT_INVALID | RESULT_UNSPECIFIED => continue,
                RESULT_TRUST_ROOT | RESULT_TRUST_AS_ROOT => return Some(Trust::Root),
                // Denied, or a value this code does not know.
                _ => return Some(Trust::No),
            }
        }
    }
    if any {
        None
    } else {
        Some(Trust::Root)
    }
}

/// The certificates the keychain trusts as roots for TLS servers, in DER.
/// Empty when the trust settings cannot be read.
pub(crate) fn trusted_roots() -> Vec<CertificateDer<'static>> {
    let Some(keys) = Keys::new() else {
        return Vec::new();
    };
    // By certificate, what the first domain that has a say said.
    let mut seen: std::collections::HashMap<Vec<u8>, Trust> = std::collections::HashMap::new();
    let mut order = Vec::new();
    for domain in DOMAINS {
        let mut certs: CFTypeRef = ptr::null();
        // SAFETY: `certs` receives an array we own, or stays null. A domain
        // without settings reports an error, and is skipped.
        let status = unsafe { SecTrustSettingsCopyCertificates(domain, &mut certs) };
        let Some(certs) = Owned::new(certs).filter(|_| status == 0) else {
            continue;
        };
        for cert in elements(&certs) {
            if cert.is_null() {
                continue;
            }
            // SAFETY: `cert` is a certificate of the array, alive while the
            // array is; the data is a copy we own, read within its length.
            let der = unsafe {
                let Some(data) = Owned::new(SecCertificateCopyData(cert)) else {
                    continue;
                };
                let (bytes, len) = (CFDataGetBytePtr(data.0), CFDataGetLength(data.0));
                if bytes.is_null() || len <= 0 {
                    continue;
                }
                std::slice::from_raw_parts(bytes, len as usize).to_vec()
            };
            if seen.contains_key(&der) {
                continue;
            }
            let mut settings: CFTypeRef = ptr::null();
            // SAFETY: `settings` receives an array we own, or stays null.
            let status = unsafe { SecTrustSettingsCopyTrustSettings(cert, domain, &mut settings) };
            let trust = match Owned::new(settings).filter(|_| status == 0) {
                Some(settings) => evaluate(&settings, &keys),
                // Listed in the domain, but its settings cannot be read:
                // not trusted on a guess.
                None => Some(Trust::No),
            };
            if let Some(trust) = trust {
                seen.insert(der.clone(), trust);
                order.push(der);
            }
        }
    }
    order
        .into_iter()
        .filter(|der| seen.get(der) == Some(&Trust::Root))
        .map(CertificateDer::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every Mac ships Apple's root certificates in the system domain.
    #[test]
    fn the_keychain_has_roots_and_rustls_can_use_them() {
        let roots = trusted_roots();
        assert!(roots.len() > 50, "only {} roots", roots.len());
        let mut store = rustls::RootCertStore::empty();
        let (added, _ignored) = store.add_parsable_certificates(roots);
        assert!(added > 50, "rustls took only {added}");
    }

    #[test]
    fn dictionary_keys_can_be_made() {
        let keys = Keys::new().unwrap();
        // SAFETY: a live string compared with itself and with another.
        unsafe {
            assert_ne!(CFEqual(keys.policy.0, keys.policy.0), 0);
            assert_eq!(CFEqual(keys.policy.0, keys.result.0), 0);
        }
    }
}
