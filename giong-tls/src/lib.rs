//! # giong-tls
//!
//! TLS for the [giong] thread-per-core runtime, built on [rustls] (TLS 1.2
//! and 1.3, `ring` for cryptography, no OpenSSL): [`TlsAcceptor`] for
//! servers, [`TlsConnector`] for clients. Both produce a [`TlsStream`].
//!
//! A server:
//!
//! ```no_run
//! use giong::net::TcpListener;
//! use giong_tls::{Certificates, TlsAcceptor};
//!
//! # fn main() -> std::io::Result<()> {
//! let cert = std::fs::read("cert.pem")?;
//! let key = std::fs::read("key.pem")?;
//! let acceptor = TlsAcceptor::with_certificates(&Certificates::from_pem(&cert, &key)?)?;
//!
//! giong::block_on(async move {
//!     let listener = TcpListener::bind("0.0.0.0:8443")?;
//!     loop {
//!         let tcp = listener.accept().await?;
//!         let acceptor = acceptor.clone();
//!         giong::spawn(async move {
//!             let Ok(tls) = acceptor.accept(tcp).await else { return };
//!             while let Ok(data) = tls.recv().await {
//!                 if data.is_empty() {
//!                     break; // closed by the peer
//!                 }
//!                 if tls.write_all(data).await.0.is_err() {
//!                     break;
//!                 }
//!             }
//!             let _ = tls.shutdown().await;
//!         });
//!     }
//! })
//! # }
//! ```
//!
//! A client, trusting the certificate authorities installed on the machine:
//!
//! ```no_run
//! use giong::net::TcpStream;
//! use giong_tls::TlsConnector;
//!
//! # fn main() -> std::io::Result<()> {
//! let connector = TlsConnector::with_system_roots()?;
//!
//! giong::block_on(async move {
//!     let tcp = TcpStream::connect("example.com:443").await?;
//!     let tls = connector.connect("example.com", tcp).await?;
//!     let request = b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n";
//!     tls.write_all(request.to_vec()).await.0?;
//!     loop {
//!         let data = tls.recv().await?;
//!         if data.is_empty() {
//!             break;
//!         }
//!         print!("{}", String::from_utf8_lossy(&data));
//!     }
//!     Ok(())
//! })
//! # }
//! ```
//!
//! The API mirrors [`giong::net::TcpStream`]: [`TlsStream::recv`] returns the
//! next piece of decrypted data and [`TlsStream::write_all`] takes a buffer by
//! value and hands it back.
//!
//! # Kernel TLS
//!
//! With [`TlsAcceptor::ktls`] or [`TlsConnector::ktls`], rustls does the
//! handshake and then hands the connection's keys to the kernel (kTLS), which
//! encrypts and decrypts from then on. The API stays the same;
//! [`TlsStream::is_ktls`] says which way a connection went, [`ktls_support`]
//! what the running kernel can do, and [`ktls_stats`] how many connections
//! went to the kernel.

#![warn(missing_docs)]

mod certs;
mod ktls;
mod received;

pub use certs::Certificates;
pub use ktls::{ktls_stats, ktls_support, Ktls, KtlsStats, KtlsSupport};
pub use received::Received;
/// A certificate in DER form, as [`TlsStream::peer_certificates`] returns
/// it: from `rustls-pki-types` 1.x, which stays at 1.x whatever rustls does.
pub use rustls_pki_types::CertificateDer;

/// Entry points for the fuzz targets in `giong-tls/fuzz`. Only built by
/// `cargo fuzz` (`--cfg fuzzing`); not part of the API.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    pub use crate::ktls::{check_handshake_messages, check_record_framing};
}

use std::cell::{Cell, RefCell, RefMut};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::future::poll_fn;
use std::io::{self, Read, Write};
use std::mem;
use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

use giong::error::Code;
use giong::net::TcpStream;
use giong::time::timeout;
use rustls::client::ResolvesClientCert;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{PrivateKeyDer, ServerName};
use rustls::sign::CertifiedKey;
use rustls::{
    ClientConfig, ClientConnection, Connection, RootCertStore, ServerConfig, ServerConnection,
    SignatureScheme,
};

/// The rustls crate, for building the configurations that
/// [`TlsAcceptor::from_rustls`] and [`TlsConnector::from_rustls`] take.
///
/// Needs the `unstable-rustls` feature. Outside the semver promise: it is
/// the rustls version giong-tls uses, which a minor release may raise.
#[cfg(feature = "unstable-rustls")]
pub use rustls;

fn coded(
    code: Code,
    message: impl Into<std::borrow::Cow<'static, str>>,
    fix: &'static str,
) -> io::Error {
    giong::error::Error::new(code, message, fix).into()
}

/// `send_file` was asked for bytes past the end of the file; it stopped at
/// `at`.
fn file_too_short(offset: u64, len: u64, at: u64) -> io::Error {
    let end = offset.saturating_add(len);
    coded(
        Code::FileTooShort,
        format!(
            "the file ended at byte {at}, {} bytes short of the {len} send_file was asked to send",
            end - at
        ),
        "take the length from the open file (File::len), and replace a file that is being \
         served by renaming a new one over it, not by rewriting it; the peer has fewer bytes \
         than announced, so close the connection.",
    )
}

/// PEM data or a certificate file that cannot be used.
pub(crate) fn bad_certificate_data(e: impl std::fmt::Display) -> io::Error {
    coded(
        Code::BadCertificateData,
        e.to_string(),
        "check that the file or data is PEM (\"-----BEGIN CERTIFICATE-----\") and holds \
         at least one certificate.",
    )
}

/// A private key that cannot be read.
pub(crate) fn bad_key(e: impl std::fmt::Display) -> io::Error {
    coded(
        Code::BadCertificateData,
        format!("cannot read the private key: {e}"),
        "check that it is a PEM private key (\"-----BEGIN PRIVATE KEY-----\", or the RSA or \
         EC forms).",
    )
}

/// A certificate and a key that cannot be used together.
pub(crate) fn mismatched_key(e: rustls::Error) -> io::Error {
    coded(
        Code::BadCertificateData,
        format!("the certificate and the private key cannot be used together: {e}"),
        "check that the key belongs to the first certificate of the chain, and that its type \
         (RSA, ECDSA, Ed25519) is supported.",
    )
}

/// What rustls reported, with the code that says what to do about it.
fn tls_error(e: rustls::Error) -> io::Error {
    match e {
        // What `openssl req -x509` makes unless told otherwise, and what
        // rustls reports only as a debug value.
        rustls::Error::InvalidCertificate(_) if e.to_string().contains("CaUsedAsEndEntity") => {
            coded(
                Code::TlsCertificate,
                "the peer's certificate was rejected: it is marked as a certificate authority \
                 (basicConstraints CA:TRUE), and an authority's certificate cannot be the \
                 peer's own (CaUsedAsEndEntity)",
                "issue the peer's certificate with CA:FALSE: a self-signed one made with \
                 `openssl req -x509` needs -addext \"basicConstraints=critical,CA:FALSE\"; or \
                 have the authority sign a separate certificate for the peer.",
            )
        }
        rustls::Error::InvalidCertificate(_) => coded(
            Code::TlsCertificate,
            format!("the peer's certificate was rejected: {e}"),
            "check the name you connect to, the certificate's expiry and its chain; a \
             private authority has to be trusted with TlsConnector::with_root_pem.",
        ),
        e => coded(
            Code::TlsProtocol,
            format!("TLS error: {e}"),
            "check that the other side speaks TLS on this port, and that both sides share a \
             protocol version and cipher suite.",
        ),
    }
}

/// How long a handshake may take unless configured otherwise.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

fn handshake_timed_out(limit: Duration) -> io::Error {
    coded(
        Code::TlsHandshakeTimeout,
        format!("the TLS handshake did not finish within {limit:?}"),
        "check that the other side speaks TLS on this port and is not overloaded, or raise \
         handshake_timeout.",
    )
}

fn handshake_eof() -> io::Error {
    coded(
        Code::TlsHandshakeEof,
        "the peer closed the connection during the TLS handshake",
        "check that the other side speaks TLS on this port, and whether it wants a client \
         certificate or another server name.",
    )
}

/// Most capacity the scratch buffer for outgoing records keeps between
/// writes. rustls hands over up to 64 KiB of records per call; a buffer that
/// grew to that is dropped after the write instead of being kept for the
/// connection's whole life (about 64 KiB per idle connection otherwise).
const WIRE_KEPT: usize = 16 * 1024;

/// Encrypted records of one write that are sent as soon as this many have
/// gathered, even if more pieces of the write follow.
const SEND_AT: usize = 64 * 1024;

#[cfg(test)]
thread_local! {
    /// How many bytes one [`TlsStream::flush_now`] may hand to the socket,
    /// for tests of a socket that takes a record in pieces.
    static FLUSH_ROOM: Cell<usize> = const { Cell::new(usize::MAX) };
}

/// How many bytes one `flush_now` may hand to the socket: no limit outside
/// tests.
fn flush_room() -> usize {
    #[cfg(test)]
    return FLUSH_ROOM.with(Cell::get);
    #[cfg(not(test))]
    usize::MAX
}

/// `true` if `records`, whole TLS records as rustls writes them, hold one
/// of application data (content type 23; under TLS 1.3 every encrypted
/// record says so).
fn has_application_data(records: &[u8]) -> bool {
    let mut at = 0;
    while let Some(header) = records.get(at..at + 5) {
        if header[0] == 23 {
            return true;
        }
        at += 5 + usize::from(u16::from_be_bytes([header[3], header[4]]));
    }
    false
}

/// The error a send fails with once the sending direction has ended for
/// `code`'s reason.
fn sending_closed(code: Code) -> io::Error {
    match code {
        Code::TlsKeysExhausted => coded(
            Code::TlsKeysExhausted,
            "this connection has sent as many TLS records as its cipher allows under one key, \
             and cannot change keys (TLS 1.2, or TLS 1.3 on a kernel older than 6.14); \
             close_notify was sent",
            "open a new connection. TLS 1.3 changes keys instead (in the process, and with \
             kernel TLS on Linux 6.14 or newer).",
        ),
        _ => coded(
            Code::TlsBroken,
            "this TLS stream has been shut down or ended with an alert; nothing more can be sent",
            "open a new connection.",
        ),
    }
}

fn broken() -> io::Error {
    coded(
        Code::TlsBroken,
        "a TLS write failed or was cancelled; nothing more can be sent",
        "open a new connection. A write cancelled half-way (a dropped future, a timeout) \
         leaves a partial record or message behind, so the stream cannot go on.",
    )
}

/// Every certificate in `pem`, or an error if there is none.
pub(crate) fn pem_certs(pem: &[u8]) -> io::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(bad_certificate_data)?;
    if certs.is_empty() {
        return Err(bad_certificate_data("no certificate found in the PEM data"));
    }
    Ok(certs)
}

/// The cryptography every configuration giong-tls builds uses: *ring*, or
/// aws-lc-rs with the `post-quantum` feature, whose key exchange offers
/// X25519MLKEM768 (a post-quantum hybrid) first.
pub(crate) fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(base_provider())
}

#[cfg(not(feature = "post-quantum"))]
fn base_provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

#[cfg(feature = "post-quantum")]
fn base_provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

/// [`provider`] with AES-128-GCM first, then ChaCha20-Poly1305, then AES-256-GCM,
/// in each protocol version: the order of [`TlsAcceptor::with_certificates`].
///
/// AES-128-GCM is as safe in practice and cheaper: its suites hash with
/// SHA-256, which processors run in hardware, where AES-256-GCM's TLS 1.3
/// suite and TLS 1.2 ECDHE suites hash with SHA-384 (about a sixth of a
/// resumed handshake's CPU time goes to SHA-384 otherwise). Go's and
/// BoringSSL's servers prefer it the same way. Browsers list it first
/// already; OpenSSL clients (curl, wrk, Python) list AES-256-GCM first.
fn provider_aes128_first() -> Arc<rustls::crypto::CryptoProvider> {
    use rustls::CipherSuite as S;
    let mut provider = base_provider();
    // A stable sort: TLS 1.3 suites stay ahead of TLS 1.2 ones, and the
    // signature algorithms keep their order.
    provider.cipher_suites.sort_by_key(|suite| {
        let rank = match suite.suite() {
            S::TLS13_AES_128_GCM_SHA256
            | S::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
            | S::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256 => 0,
            S::TLS13_CHACHA20_POLY1305_SHA256
            | S::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256
            | S::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256 => 1,
            _ => 2,
        };
        (
            suite.version().version != rustls::ProtocolVersion::TLSv1_3,
            rank,
        )
    });
    Arc::new(provider)
}

/// `true` if a ClientHello lists a ChaCha20-Poly1305 suite before every
/// AES-GCM one: a client without AES instructions (a low-end phone) says
/// so, and then gets ChaCha20, which it runs several times faster. (The
/// same test as Go's server.) OpenSSL's clients list AES-256-GCM, then
/// ChaCha20, then AES-128-GCM, and get AES-128-GCM.
/// `hello` is the start of what the client sent; `false` if it is not a
/// ClientHello or ends before the list of cipher suites does.
fn prefers_chacha(hello: &[u8]) -> bool {
    // Record header (5), handshake header (4), version (2), random (32).
    const SESSION_ID: usize = 5 + 4 + 2 + 32;
    if hello.first() != Some(&22) || hello.get(5) != Some(&1) {
        return false;
    }
    let Some(&id_len) = hello.get(SESSION_ID) else {
        return false;
    };
    let at = SESSION_ID + 1 + usize::from(id_len);
    let Some(len) = hello.get(at..at + 2) else {
        return false;
    };
    let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
    let Some(suites) = hello.get(at + 2..at + 2 + len) else {
        return false;
    };
    for suite in suites.chunks_exact(2) {
        match [suite[0], suite[1]] {
            // TLS_AES_128_GCM_SHA256, TLS_AES_256_GCM_SHA384,
            // ECDHE_ECDSA_/ECDHE_RSA_WITH_AES_128_GCM_SHA256 and _256_GCM_SHA384
            [0x13, 0x01]
            | [0x13, 0x02]
            | [0xc0, 0x2b]
            | [0xc0, 0x2f]
            | [0xc0, 0x2c]
            | [0xc0, 0x30] => return false,
            // TLS_CHACHA20_POLY1305_SHA256, ECDHE_ECDSA_/ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256
            [0x13, 0x03] | [0xcc, 0xa9] | [0xcc, 0xa8] => return true,
            _ => {}
        }
    }
    false
}

/// The two configurations of a [`TlsAcceptor`] built from `certificates`:
/// the server's cipher suite order (AES-128-GCM first), and a copy in the
/// client's order, for clients that list ChaCha20 first (see
/// [`prefers_chacha`]).
fn server_configs(
    certificates: &Certificates,
    verifier: Arc<dyn rustls::server::danger::ClientCertVerifier>,
) -> io::Result<(ServerConfig, ServerConfig)> {
    let mut config = ServerConfig::builder_with_provider(provider_aes128_first())
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(certificates.resolver());
    let chacha = config.clone();
    config.ignore_client_order = true;
    Ok((config, chacha))
}

/// Turns accepted TCP connections into TLS connections.
///
/// Cheap to clone; share one between all workers.
#[derive(Clone)]
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
    /// From [`with_certificates`](Self::with_certificates): `config` picks the server's
    /// favourite cipher suite (AES-128-GCM), and this copy of it the
    /// client's, for clients that list ChaCha20 first (see
    /// [`prefers_chacha`]).
    chacha: Option<Arc<ServerConfig>>,
    handshake_timeout: Duration,
    ktls: ktls::Plan,
    /// From [`with_certificates`](Self::with_certificates): what the
    /// configurations were built from, to build them again with client
    /// authentication. `None` for [`from_rustls`](Self::from_rustls).
    certificates: Option<Certificates>,
}

impl TlsAcceptor {
    /// An acceptor using an existing rustls configuration, for what
    /// [`with_certificates`](Self::with_certificates) and the methods below
    /// do not cover. The acceptor follows the configuration as given.
    ///
    /// Needs the `unstable-rustls` feature. Outside the semver promise: the
    /// configuration is the rustls version giong-tls uses
    /// ([`rustls`]), which a minor release may raise.
    #[cfg(feature = "unstable-rustls")]
    pub fn from_rustls(config: Arc<ServerConfig>) -> Self {
        Self::with_config(config)
    }

    fn with_config(config: Arc<ServerConfig>) -> Self {
        Self {
            config,
            chacha: None,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ktls: ktls::Plan::Off,
            certificates: None,
        }
    }

    /// An acceptor presenting `certificates`: one chain, or several chosen
    /// by the name the client asks for (SNI), replaceable while the server
    /// runs (see [`Certificates`]). TLS 1.2 and 1.3 with rustls' default
    /// cipher suites, no client authentication, and no protocol offered
    /// through ALPN: set them with [`alpn`](Self::alpn). (giong-http's
    /// `Server::serve_tls` sets its own.)
    ///
    /// Of the suites both sides allow, the server picks AES-128-GCM first,
    /// as Go's and BoringSSL's servers do: it costs less than AES-256-GCM,
    /// whose suites hash with SHA-384. A client that lists ChaCha20-Poly1305
    /// before AES-128-GCM, as clients without AES instructions do, gets
    /// ChaCha20. An acceptor made with `from_rustls` follows its
    /// configuration as given.
    ///
    /// ```no_run
    /// use giong_tls::{Certificates, TlsAcceptor};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let certs = Certificates::from_pem(&std::fs::read("cert.pem")?, &std::fs::read("key.pem")?)?;
    /// let acceptor = TlsAcceptor::with_certificates(&certs)?.alpn(&[b"h2", b"http/1.1"]);
    /// # let _ = acceptor;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_certificates(certificates: &Certificates) -> io::Result<Self> {
        let verifier = rustls::server::WebPkiClientVerifier::no_client_auth();
        let (config, chacha) = server_configs(certificates, verifier)?;
        Ok(Self {
            chacha: Some(Arc::new(chacha)),
            certificates: Some(certificates.clone()),
            ..Self::with_config(Arc::new(config))
        })
    }

    /// Requires every client to present a certificate issued by one of the
    /// authorities in `ca_pem` (mutual TLS): a client without one, or with
    /// another, fails the handshake. The accepted client's certificates are
    /// [`TlsStream::peer_certificates`], the client's own first.
    ///
    /// For acceptors made with
    /// [`with_certificates`](Self::with_certificates); one made with
    /// `from_rustls` sets client authentication in its configuration, and
    /// gets `GIONG-E2013` here.
    pub fn require_client_cert(self, ca_pem: &[u8]) -> io::Result<Self> {
        self.client_auth(ca_pem, true)
    }

    /// Asks clients for a certificate issued by one of the authorities in
    /// `ca_pem`, and accepts clients that send none:
    /// [`TlsStream::peer_certificates`] then says which did. A client that
    /// sends a certificate the authorities did not issue still fails the
    /// handshake. Otherwise as
    /// [`require_client_cert`](Self::require_client_cert).
    pub fn request_client_cert(self, ca_pem: &[u8]) -> io::Result<Self> {
        self.client_auth(ca_pem, false)
    }

    fn client_auth(mut self, ca_pem: &[u8], required: bool) -> io::Result<Self> {
        let Some(certificates) = &self.certificates else {
            return Err(coded(
                Code::TlsConfig,
                "client certificates cannot be set on an acceptor made with from_rustls",
                "set client authentication in the rustls configuration given to from_rustls \
                 (ServerConfig::builder().with_client_cert_verifier), or make the acceptor \
                 with with_certificates.",
            ));
        };
        let mut roots = RootCertStore::empty();
        for cert in pem_certs(ca_pem)? {
            roots.add(cert).map_err(bad_certificate_data)?;
        }
        let builder = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            self.config.crypto_provider().clone(),
        );
        let builder = if required {
            builder
        } else {
            builder.allow_unauthenticated()
        };
        let verifier = builder.build().map_err(bad_certificate_data)?;
        let (mut config, mut chacha) = server_configs(certificates, verifier)?;
        // What the other builder methods set: ALPN, and kTLS's secrets.
        for new in [&mut config, &mut chacha] {
            new.alpn_protocols = self.config.alpn_protocols.clone();
            new.enable_secret_extraction = self.config.enable_secret_extraction;
        }
        self.config = Arc::new(config);
        self.chacha = Some(Arc::new(chacha));
        Ok(self)
    }

    /// The configuration for a client whose ClientHello starts with
    /// `hello`.
    fn config_for(&self, hello: &[u8]) -> Arc<ServerConfig> {
        match &self.chacha {
            Some(chacha) if prefers_chacha(hello) => chacha.clone(),
            _ => self.config.clone(),
        }
    }

    /// Applies `change` to every configuration this acceptor holds.
    fn change_config(&mut self, change: impl Fn(&mut ServerConfig)) {
        change(Arc::make_mut(&mut self.config));
        if let Some(chacha) = &mut self.chacha {
            change(Arc::make_mut(chacha));
        }
    }

    /// The protocols to accept through ALPN, most preferred first, for
    /// example `&[b"h2", b"http/1.1"]`. A client that offers none of them
    /// fails the handshake; one that offers no ALPN at all is accepted.
    /// The protocol agreed on is [`TlsStream::alpn_protocol`].
    ///
    /// giong-http's `Server::serve_tls` sets this itself, to the protocols
    /// the server speaks.
    pub fn alpn(mut self, protocols: &[&[u8]]) -> Self {
        let protocols: Vec<Vec<u8>> = protocols.iter().map(|p| p.to_vec()).collect();
        self.change_config(|config| config.alpn_protocols = protocols.clone());
        self
    }

    /// How long a client may take to complete the TLS handshake (default:
    /// 10 s).
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Whether connections hand their keys to the kernel after the
    /// handshake, so that the kernel encrypts and decrypts (kernel TLS).
    /// [`Ktls::Off`] by default. See [`Ktls`] for what each mode does, and
    /// the crate README for when it pays off.
    ///
    /// Decided here, once: the running kernel is probed (see
    /// [`ktls_support`]) and checked against every cipher suite the
    /// configuration allows. [`Ktls::Required`] fails here with `GIONG-E2009`
    /// if the kernel cannot take them; [`Ktls::Auto`] then leaves every
    /// connection in the process instead. A configuration that allows 0-RTT
    /// data (`max_early_data_size` above 0) cannot use kTLS.
    ///
    /// Turns on rustls' `enable_secret_extraction`, which kTLS needs, in this
    /// acceptor's copy of the configuration, when connections will try the
    /// kernel (not when `Auto` finds the kernel unable).
    ///
    /// ```no_run
    /// use giong_tls::{Certificates, Ktls, TlsAcceptor};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let certs = Certificates::from_pem(&std::fs::read("cert.pem")?, &std::fs::read("key.pem")?)?;
    /// let acceptor = TlsAcceptor::with_certificates(&certs)?
    ///     .ktls(Ktls::Auto)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn ktls(mut self, mode: Ktls) -> io::Result<Self> {
        let early_data = self.config.max_early_data_size > 0;
        self.ktls = ktls::Plan::new(mode, self.config.crypto_provider(), early_data)?;
        // Extractable secrets only where they will be handed to the kernel.
        let extract = self.ktls.tries();
        if self.config.enable_secret_extraction != extract {
            self.change_config(|config| config.enable_secret_extraction = extract);
        }
        Ok(self)
    }

    /// Why connections stay in the process although [`Ktls::Auto`] asked for
    /// kernel TLS, if they all do: the kernel has no `tls` module, lacks a
    /// cipher suite the configuration allows, or cannot change TLS 1.3 keys.
    /// `None` otherwise. Worth a line in the log at startup.
    pub fn ktls_unavailable(&self) -> Option<&str> {
        self.ktls.unavailable()
    }

    /// Performs the TLS handshake on an accepted connection.
    ///
    /// With kernel TLS (see [`ktls`](Self::ktls)), the keys go to the kernel
    /// once the handshake is done. Under [`Ktls::Required`], a stream that
    /// cannot take them fails with `GIONG-E2009` before anything is sent.
    ///
    /// Cancel safe: if the future is dropped, the TCP connection it took is
    /// closed; nothing else changes.
    pub async fn accept(&self, tcp: TcpStream) -> io::Result<TlsStream> {
        let tcp = match &self.ktls {
            ktls::Plan::Off => tcp,
            ktls::Plan::Try { required } => {
                let start = ktls::Start::Server(self.config.clone(), self.chacha.clone());
                match ktls::handshake(tcp, start, self.handshake_timeout).await? {
                    ktls::Outcome::Kernel(tls) => return Ok(*tls),
                    ktls::Outcome::Declined(_, e) if *required => return Err(e),
                    ktls::Outcome::Declined(tcp, _) => {
                        self.ktls.stayed();
                        tcp
                    }
                }
            }
            ktls::Plan::Unavailable(_) => {
                self.ktls.stayed();
                tcp
            }
        };
        let Some(_) = &self.chacha else {
            let conn = ServerConnection::new(self.config.clone()).map_err(tls_error)?;
            return TlsStream::new(tcp, conn.into())
                .handshake(self.handshake_timeout, Duration::ZERO, None)
                .await;
        };
        // The configuration depends on the ClientHello: wait for its start,
        // within the handshake's time.
        let started = std::time::Instant::now();
        let first = match timeout(self.handshake_timeout, tcp.recv()).await {
            Ok(first) => first?,
            Err(_) => return Err(handshake_timed_out(self.handshake_timeout)),
        };
        if first.is_empty() {
            return Err(handshake_eof());
        }
        let conn = ServerConnection::new(self.config_for(&first)).map_err(tls_error)?;
        TlsStream::new(tcp, conn.into())
            .handshake(self.handshake_timeout, started.elapsed(), Some(first))
            .await
    }
}

impl std::fmt::Debug for TlsAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsAcceptor")
            .field("handshake_timeout", &self.handshake_timeout)
            .field("ktls", &self.ktls)
            .finish_non_exhaustive()
    }
}

/// Opens TLS connections to servers, checking their certificates.
///
/// Cheap to clone; share one between all workers. Clones share the session
/// cache, so reconnecting to a server resumes the earlier session and skips
/// most of the handshake.
///
/// ```no_run
/// use std::time::Duration;
/// use giong_tls::TlsConnector;
///
/// # fn main() -> std::io::Result<()> {
/// // A private certificate authority, a client certificate for mutual TLS,
/// // and HTTP/1.1 through ALPN.
/// let connector = TlsConnector::with_root_pem(&std::fs::read("ca.pem")?)?
///     .client_auth(&std::fs::read("client.pem")?, &std::fs::read("client.key")?)?
///     .alpn(&[b"http/1.1"])
///     .handshake_timeout(Duration::from_secs(5));
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct TlsConnector {
    config: Arc<ClientConfig>,
    handshake_timeout: Duration,
    ktls: ktls::Plan,
}

impl TlsConnector {
    /// A connector using an existing rustls configuration, for what the
    /// other constructors do not cover (a certificate verifier of its own,
    /// other protocol versions).
    ///
    /// Needs the `unstable-rustls` feature. Outside the semver promise: the
    /// configuration is the rustls version giong-tls uses
    /// ([`rustls`]), which a minor release may raise.
    #[cfg(feature = "unstable-rustls")]
    pub fn from_rustls(config: Arc<ClientConfig>) -> Self {
        Self::with_config(config)
    }

    fn with_config(config: Arc<ClientConfig>) -> Self {
        Self {
            config,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ktls: ktls::Plan::Off,
        }
    }

    fn from_roots(roots: RootCertStore) -> io::Result<Self> {
        if roots.is_empty() {
            return Err(coded(
                Code::NoTrustedRoots,
                "no trusted certificate authority given",
                "pass at least one certificate, or use TlsConnector::with_system_roots.",
            ));
        }
        let config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(tls_error)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self::with_config(Arc::new(config)))
    }

    /// A connector that trusts the certificate authorities in `pem`: a
    /// private CA, or a server's own self-signed certificate.
    ///
    /// A self-signed certificate must not be marked as an authority
    /// (`CA:TRUE`), which `openssl req -x509` does unless it is given
    /// `-addext "basicConstraints=critical,CA:FALSE"`: a server that
    /// presents an authority's certificate as its own is refused with
    /// `GIONG-E2004`, whether the certificate is trusted or not.
    pub fn with_root_pem(pem: &[u8]) -> io::Result<Self> {
        let mut roots = RootCertStore::empty();
        for cert in pem_certs(pem)? {
            roots.add(cert).map_err(bad_certificate_data)?;
        }
        Self::from_roots(roots)
    }

    /// A connector that trusts the certificate authorities in `pem` and
    /// accepts a certificate they signed **whatever host name it is for**:
    /// the chain, the validity period and the key usage are checked, the
    /// name is not.
    ///
    /// For servers whose certificates name no host, such as the ones MySQL
    /// generates when it is installed (`ssl-mode=VERIFY_CA`): pass the
    /// server's own authority. Use it only with an authority that signs for
    /// this server alone; with a public one, any certificate it issued, for
    /// any name, would pass. Where the certificate names the host, use
    /// [`with_root_pem`](Self::with_root_pem), which checks both.
    pub fn with_root_pem_any_name(pem: &[u8]) -> io::Result<Self> {
        let mut roots = RootCertStore::empty();
        for cert in pem_certs(pem)? {
            roots.add(cert).map_err(bad_certificate_data)?;
        }
        if roots.is_empty() {
            return Err(coded(
                Code::NoTrustedRoots,
                "no trusted certificate authority given",
                "pass at least one certificate authority in PEM form.",
            ));
        }
        let provider = provider();
        let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .map_err(bad_certificate_data)?;
        let config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(tls_error)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyName(webpki)))
            .with_no_client_auth();
        Ok(Self::with_config(Arc::new(config)))
    }

    /// A connector that trusts the certificate authorities installed on this
    /// machine, read once, now.
    ///
    /// They are read from the file named by the `SSL_CERT_FILE` environment
    /// variable and the directories named by `SSL_CERT_DIR` (separated by
    /// `:`) if either is set, the way OpenSSL does. Otherwise from the first
    /// bundle found among the places Linux distributions keep it
    /// (`/etc/ssl/certs/ca-certificates.crt` on Debian, Ubuntu, Alpine and
    /// Arch, `/etc/pki/tls/certs/ca-bundle.crt` on Fedora and RHEL, and so
    /// on), or else from the files in `/etc/ssl/certs`.
    ///
    /// Fails with [`io::ErrorKind::NotFound`] if there are none, as in a
    /// container image built `FROM scratch` or without the `ca-certificates`
    /// package. Install that package, or enable the `webpki-roots` feature and
    /// use `TlsConnector::with_webpki_roots`.
    pub fn with_system_roots() -> io::Result<Self> {
        let file = std::env::var_os("SSL_CERT_FILE");
        let dirs = std::env::var_os("SSL_CERT_DIR");
        let roots = load_roots(
            file.as_deref(),
            dirs.as_deref(),
            SYSTEM_CA_BUNDLES,
            SYSTEM_CA_DIR,
        )?;
        Self::from_roots(roots)
    }

    /// A connector that trusts the certificate authorities in Mozilla's root
    /// program, compiled into the program by the `webpki-roots` crate. Useful
    /// where the machine has none installed; they change only when this
    /// crate's dependency is updated.
    ///
    /// Needs the `webpki-roots` feature.
    #[cfg(feature = "webpki-roots")]
    pub fn with_webpki_roots() -> Self {
        let roots = RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        Self::from_roots(roots).expect("the webpki-roots crate provides roots")
    }

    /// The protocols to offer through ALPN, most preferred first, for example
    /// `&[b"h2", b"http/1.1"]`. None by default. The one the server picked is
    /// [`TlsStream::alpn_protocol`].
    pub fn alpn(mut self, protocols: &[&[u8]]) -> Self {
        Arc::make_mut(&mut self.config).alpn_protocols =
            protocols.iter().map(|p| p.to_vec()).collect();
        self
    }

    /// Presents a client certificate to servers that ask for one (mutual
    /// TLS): a certificate chain and its private key, both in PEM format.
    pub fn client_auth(mut self, cert_chain: &[u8], private_key: &[u8]) -> io::Result<Self> {
        let certs = pem_certs(cert_chain)?;
        let key = PrivateKeyDer::from_pem_slice(private_key).map_err(bad_key)?;
        let key = CertifiedKey::from_der(certs, key, self.config.crypto_provider())
            .map_err(mismatched_key)?;
        Arc::make_mut(&mut self.config).client_auth_cert_resolver =
            Arc::new(ClientCert(Arc::new(key)));
        Ok(self)
    }

    /// How long a server may take to complete the TLS handshake (default:
    /// 10 s).
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Whether connections hand their keys to the kernel after the
    /// handshake, so that the kernel encrypts and decrypts (kernel TLS).
    /// [`Ktls::Off`] by default; see [`TlsAcceptor::ktls`], which works the
    /// same way. Session tickets the server sends later still reach the
    /// session cache, so reconnecting resumes as without kTLS. A
    /// configuration that allows 0-RTT data (`enable_early_data`) cannot use
    /// kTLS.
    ///
    /// A stream on which [`TcpStream::recv`] was already used, as by a
    /// protocol that reads a plaintext greeting before TLS starts, cannot
    /// take kTLS: under [`Ktls::Auto`] it stays in the process, under
    /// [`Ktls::Required`] it fails with `GIONG-E2009`. Use
    /// [`TcpStream::read`] for such greetings.
    pub fn ktls(mut self, mode: Ktls) -> io::Result<Self> {
        let early_data = self.config.enable_early_data;
        self.ktls = ktls::Plan::new(mode, self.config.crypto_provider(), early_data)?;
        let extract = self.ktls.tries();
        if self.config.enable_secret_extraction != extract {
            Arc::make_mut(&mut self.config).enable_secret_extraction = extract;
        }
        Ok(self)
    }

    /// Why connections stay in the process although [`Ktls::Auto`] asked for
    /// kernel TLS, if they all do; see [`TlsAcceptor::ktls_unavailable`].
    pub fn ktls_unavailable(&self) -> Option<&str> {
        self.ktls.unavailable()
    }

    /// Performs the TLS handshake on a connection to `server_name`, which the
    /// server's certificate must be valid for. `server_name` is a DNS name,
    /// sent to the server through SNI, or an IP address (`[...]` brackets
    /// around an IPv6 address are allowed), which is not.
    ///
    /// A certificate that is not valid for the name, expired, or not issued
    /// by a trusted authority fails the handshake with
    /// [`io::ErrorKind::InvalidData`] and a message saying which it was.
    ///
    /// Cancel safe: if the future is dropped, the TCP connection it took is
    /// closed; nothing else changes.
    pub async fn connect(&self, server_name: &str, tcp: TcpStream) -> io::Result<TlsStream> {
        let name = parse_server_name(server_name)?;
        let tcp = match &self.ktls {
            ktls::Plan::Off => tcp,
            ktls::Plan::Try { required } => {
                let start = ktls::Start::Client(self.config.clone(), name.clone());
                match ktls::handshake(tcp, start, self.handshake_timeout).await? {
                    ktls::Outcome::Kernel(tls) => return Ok(*tls),
                    ktls::Outcome::Declined(_, e) if *required => return Err(e),
                    ktls::Outcome::Declined(tcp, _) => {
                        self.ktls.stayed();
                        tcp
                    }
                }
            }
            ktls::Plan::Unavailable(_) => {
                self.ktls.stayed();
                tcp
            }
        };
        let conn = ClientConnection::new(self.config.clone(), name).map_err(tls_error)?;
        TlsStream::new(tcp, conn.into())
            .handshake(self.handshake_timeout, Duration::ZERO, None)
            .await
    }
}

impl std::fmt::Debug for TlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConnector")
            .field("handshake_timeout", &self.handshake_timeout)
            .field("ktls", &self.ktls)
            .finish_non_exhaustive()
    }
}

/// The verifier of [`TlsConnector::with_root_pem_any_name`]: checks the
/// chain like rustls' own, and accepts a certificate whose only fault is that
/// it does not name the host.
struct AnyName(Arc<rustls::client::WebPkiServerVerifier>);

impl std::fmt::Debug for AnyName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AnyName")
    }
}

impl rustls::client::danger::ServerCertVerifier for AnyName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        use rustls::CertificateError;
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            // rustls checks the name only once the chain, the validity
            // period and the key usage have passed (the test
            // `any_name_still_checks_the_chain` makes sure of it).
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// The name a client checks the server's certificate against.
fn parse_server_name(name: &str) -> io::Result<ServerName<'static>> {
    let ipv6 = name
        .strip_prefix('[')
        .and_then(|n| n.strip_suffix(']'))
        .and_then(|n| n.parse::<std::net::Ipv6Addr>().ok());
    if let Some(ip) = ipv6 {
        return Ok(ServerName::IpAddress(std::net::IpAddr::V6(ip).into()));
    }
    ServerName::try_from(name.to_owned()).map_err(|_| {
        coded(
            Code::InvalidServerName,
            format!("{name:?} is neither a DNS name nor an IP address"),
            "pass the host name the certificate is for (\"example.com\"), or an IP address, \
             without a port or a scheme.",
        )
    })
}

/// Always presents the same client certificate.
#[derive(Debug)]
struct ClientCert(Arc<CertifiedKey>);

impl ResolvesClientCert for ClientCert {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// Where Linux distributions keep the bundle of trusted CA certificates.
const SYSTEM_CA_BUNDLES: &[&str] = &[
    // Debian, Ubuntu, Alpine, Arch, Gentoo
    "/etc/ssl/certs/ca-certificates.crt",
    // Fedora, RHEL, CentOS
    "/etc/pki/tls/certs/ca-bundle.crt",
    "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
    // openSUSE, SLES
    "/etc/ssl/ca-bundle.pem",
    // Alpine, Void
    "/etc/ssl/cert.pem",
];

/// Where they are kept one per file, when there is no bundle.
const SYSTEM_CA_DIR: &str = "/etc/ssl/certs";

/// Reads trusted certificates the way OpenSSL finds them: `file` and `dirs`
/// (the `SSL_CERT_FILE` and `SSL_CERT_DIR` variables) if either is set,
/// otherwise the first of `bundles` that exists, otherwise the files in
/// `dir`. Certificates rustls cannot use are skipped.
fn load_roots(
    file: Option<&OsStr>,
    dirs: Option<&OsStr>,
    bundles: &[&str],
    dir: &str,
) -> io::Result<RootCertStore> {
    let mut roots = Roots {
        store: RootCertStore::empty(),
        seen: HashSet::new(),
    };
    if file.is_some() || dirs.is_some() {
        if let Some(file) = file {
            roots.add_file(Path::new(file)).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    giong::error::Error::new(
                        Code::BadCertificateData,
                        format!("cannot read SSL_CERT_FILE {file:?}: {e}"),
                        "point SSL_CERT_FILE at a readable PEM file of certificates, or unset \
                         it to use the system's.",
                    )
                    .with_source(e),
                )
            })?;
        }
        for dir in dirs.map(std::env::split_paths).into_iter().flatten() {
            roots.add_dir(&dir);
        }
    } else if let Some(bundle) = bundles.iter().map(Path::new).find(|p| p.is_file()) {
        roots.add_file(bundle)?;
    } else {
        roots.add_dir(Path::new(dir));
    }
    if roots.store.is_empty() {
        return Err(coded(
            Code::NoTrustedRoots,
            "no trusted CA certificates found on this machine",
            "install the ca-certificates package, set SSL_CERT_FILE, or use \
             TlsConnector::with_webpki_roots (the webpki-roots feature of giong-tls).",
        ));
    }
    Ok(roots.store)
}

struct Roots {
    store: RootCertStore,
    /// Files already read. The hashed names in certificate directories are
    /// links to other files there.
    seen: HashSet<PathBuf>,
}

impl Roots {
    fn add_file(&mut self, path: &Path) -> io::Result<()> {
        let real = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !self.seen.insert(real) {
            return Ok(());
        }
        let pem = std::fs::read(path)?;
        let certs = CertificateDer::pem_slice_iter(&pem).filter_map(Result::ok);
        self.store.add_parsable_certificates(certs);
        Ok(())
    }

    fn add_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<PathBuf> = entries.filter_map(|e| Some(e.ok()?.path())).collect();
        paths.sort();
        for path in paths.iter().filter(|p| p.is_file()) {
            // One unreadable file does not spoil the others.
            let _ = self.add_file(path);
        }
    }
}

/// A TLS connection over a [`TcpStream`], made by [`TlsAcceptor::accept`] on
/// the server side or [`TlsConnector::connect`] on the client side.
pub struct TlsStream {
    tcp: TcpStream,
    /// Who encrypts: rustls in the process, or the kernel.
    engine: RefCell<Engine>,
    /// Decrypted data not handed out yet.
    plain: RefCell<Vec<u8>>,
    /// Scratch buffer for outgoing TLS records, reused between writes while
    /// it is small (see [`WIRE_KEPT`]).
    wire: RefCell<Vec<u8>>,
    /// The handshake has returned. From then on, records rustls queues in
    /// answer to what arrives (TLS 1.3 key updates) may wait for the next
    /// send instead of going out alone (see [`hold_output`](Self::hold_output)).
    established: Cell<bool>,
    /// The peer closed the connection (with or without `close_notify`).
    eof: Cell<bool>,
    /// The peer ended its side with a `close_notify` alert, so nothing it
    /// sent was cut off.
    close_notify: Cell<bool>,
    /// A task is writing TLS records to the socket: one `write_all` (or
    /// vectored write, or file) at a time, so that neither records nor two
    /// tasks' data interleave. Other tasks wait for it in `waiters`.
    sending: Cell<bool>,
    /// A write to the socket failed or was cancelled half way: the record
    /// stream is cut in the middle and nothing more can be sent.
    broken: Cell<bool>,
    /// The sending direction ended cleanly, and why: `TlsBroken` after
    /// `shutdown` or a fatal alert, `TlsKeysExhausted` when the key may
    /// encrypt no more records and cannot be changed.
    closed: Cell<Option<Code>>,
    /// A receive error held back until the data before it has been handed out.
    failed: Cell<Option<io::Error>>,
    /// Tasks waiting for `sending` to clear.
    waiters: RefCell<Vec<Waker>>,
    /// Whole or cut records a receive began to send and the socket did not
    /// take at once (see [`flush_now`](Self::flush_now)): the next bytes this
    /// stream sends, ahead of anything rustls has queued since.
    unsent: RefCell<Vec<u8>>,
}

/// Who encrypts and decrypts a [`TlsStream`].
// Both variants are large and a stream holds one for its whole life; a box
// would cost an allocation and a pointer to follow on every call.
#[allow(clippy::large_enum_variant)]
enum Engine {
    /// rustls, in the process.
    User(Connection),
    /// The kernel; rustls keeps what it needs for key updates and session
    /// tickets.
    Kernel(ktls::Kernel),
}

/// Held while TLS records are being written to the socket. Dropped without
/// [`done`](Self::done), as when the future writing is cancelled, it marks
/// the stream broken.
struct Sending<'a> {
    stream: &'a TlsStream,
    complete: bool,
}

impl Sending<'_> {
    /// The records went out whole (or none did).
    fn done(mut self) {
        self.complete = true;
    }
}

impl Drop for Sending<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.stream.broken.set(true);
        }
        self.stream.sending.set(false);
        self.stream.wake_waiters();
    }
}

impl TlsStream {
    fn new(tcp: TcpStream, conn: Connection) -> Self {
        Self::with_engine(tcp, Engine::User(conn), Vec::new(), false)
    }

    /// A stream the kernel encrypts. `plain` is application data that arrived
    /// with the end of the handshake; `peer_closed` says `close_notify` did
    /// too.
    fn kernel(tcp: TcpStream, kernel: ktls::Kernel, plain: Vec<u8>, peer_closed: bool) -> Self {
        Self::with_engine(tcp, Engine::Kernel(kernel), plain, peer_closed)
    }

    fn with_engine(tcp: TcpStream, engine: Engine, plain: Vec<u8>, peer_closed: bool) -> Self {
        Self {
            tcp,
            engine: RefCell::new(engine),
            plain: RefCell::new(plain),
            wire: RefCell::new(Vec::new()),
            established: Cell::new(false),
            eof: Cell::new(peer_closed),
            close_notify: Cell::new(peer_closed),
            sending: Cell::new(false),
            broken: Cell::new(false),
            closed: Cell::new(None),
            failed: Cell::new(None),
            waiters: RefCell::new(Vec::new()),
            unsent: RefCell::new(Vec::new()),
        }
    }

    /// rustls' connection, on a stream rustls encrypts. Never held across
    /// an `.await`.
    fn conn(&self) -> RefMut<'_, Connection> {
        RefMut::map(self.engine.borrow_mut(), |e| match e {
            Engine::User(c) => c,
            Engine::Kernel(_) => unreachable!("rustls' connection on a kernel TLS stream"),
        })
    }

    /// `true` if the kernel encrypts and decrypts this connection (kernel
    /// TLS, see [`TlsAcceptor::ktls`]); `false` if rustls does, in the
    /// process.
    pub fn is_ktls(&self) -> bool {
        matches!(*self.engine.borrow(), Engine::Kernel(_))
    }

    /// Waits until no other task is writing records.
    async fn wait_for_sender(&self) {
        poll_fn(|cx| {
            if !self.sending.get() {
                return Poll::Ready(());
            }
            // A task polled again while it waits is listed once.
            let mut waiters = self.waiters.borrow_mut();
            if !waiters.iter().any(|w| w.will_wake(cx.waker())) {
                waiters.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await;
    }

    /// Waits for other writers, then makes this task the one writing records
    /// until the guard is dropped.
    async fn lock_send(&self) -> Sending<'_> {
        while self.sending.get() {
            self.wait_for_sender().await;
        }
        self.sending.set(true);
        Sending {
            stream: self,
            complete: false,
        }
    }

    /// Runs the handshake, either side, within what is left of `limit`
    /// after `spent`. `first` is what the peer sent first, if the caller
    /// has received it already.
    async fn handshake(
        self,
        limit: Duration,
        spent: Duration,
        first: Option<giong::RecvBuf>,
    ) -> io::Result<Self> {
        let handshake = async {
            if let Some(first) = first {
                self.absorb(first).await?;
            }
            while self.conn().is_handshaking() {
                self.flush_tls().await?;
                if !self.conn().is_handshaking() {
                    break;
                }
                if self.eof.get() {
                    return Err(handshake_eof());
                }
                self.pump().await?;
            }
            if !self.hold_output() {
                self.flush_tls().await?;
            }
            self.established.set(true);
            Ok(())
        };
        match timeout(limit.saturating_sub(spent), handshake).await {
            Ok(Ok(())) => Ok(self),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(handshake_timed_out(limit)),
        }
    }

    /// Wakes the tasks waiting for the send lock, or for their records to
    /// be sent (see [`flush_tls`](Self::flush_tls)).
    fn wake_waiters(&self) {
        let waiters = mem::take(&mut *self.waiters.borrow_mut());
        for waker in waiters {
            waker.wake();
        }
    }

    /// `true` if the records rustls has queued may wait for this stream's
    /// next send, or for a receive that has to wait (see
    /// [`pump`](Self::pump)), instead of going out alone now.
    ///
    /// That is the case once the handshake is over on a TLS 1.3 server,
    /// whose only records then are session tickets, and on either side after
    /// the handshake has returned, where they can only be key updates
    /// answering the peer's (which TLS 1.3 asks to be sent before the next
    /// data, not at once). A server's tickets then share a segment with its
    /// first response, the way most clients send their `Finished` with
    /// their first request. Never while handshaking, and never on TLS 1.2,
    /// whose server sends its last flight after the client's `Finished`:
    /// the peer waits for those records.
    fn hold_output(&self) -> bool {
        if self.established.get() {
            // TLS 1.3 has no second handshake, and rustls refuses TLS 1.2
            // renegotiation.
            return true;
        }
        let conn = self.conn();
        if conn.is_handshaking() {
            return false;
        }
        (matches!(*conn, Connection::Server(_))
            && conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3))
    }

    /// Takes the scratch buffer for outgoing records. It starts with what
    /// a receive left unsent (see [`flush_now`](Self::flush_now)), which has
    /// to go out before anything else. The caller holds the send lock.
    fn take_wire(&self) -> Vec<u8> {
        let mut wire = mem::take(&mut *self.wire.borrow_mut());
        let mut unsent = self.unsent.borrow_mut();
        if unsent.capacity() > 0 {
            wire.extend_from_slice(&unsent);
            // Not `clear`: an idle connection keeps no buffer for this.
            *unsent = Vec::new();
        }
        wire
    }

    /// Keeps `wire` for the next write, unless a large write grew it: a
    /// connection that once sent a large response should not hold that much
    /// for the rest of its life.
    fn keep_wire(&self, mut wire: Vec<u8>) {
        if !wire.is_empty() && !self.broken.get() {
            // The write ended before its first send (a file that could not
            // be read, say): what `take_wire` handed over is still owed.
            self.unsent.borrow_mut().append(&mut wire);
        }
        if wire.capacity() <= WIRE_KEPT {
            wire.clear();
            *self.wire.borrow_mut() = wire;
        }
    }

    /// The error a send fails with before it starts, if the stream cannot
    /// send any more.
    fn cannot_send(&self) -> Option<io::Error> {
        if self.broken.get() {
            return Some(broken());
        }
        self.closed.get().map(sending_closed)
    }

    /// Sends every TLS record rustls has queued: handshake messages, key
    /// updates, alerts. A task that is writing sends them with its own data
    /// (see [`send_plain`](Self::send_plain)); this waits for it.
    async fn flush_tls(&self) -> io::Result<()> {
        loop {
            if !self.wants_send() {
                return Ok(());
            }
            if self.broken.get() {
                return Err(broken());
            }
            if self.sending.get() {
                // The writer sends these records before its next piece of
                // data, and wakes us then; or it finishes and frees the lock.
                self.wait_for_sender().await;
                continue;
            }
            let sending = self.lock_send().await;
            let mut wire = self.take_wire();
            let res = self.send_queued(&mut wire).await;
            self.keep_wire(wire);
            sending.done();
            res?;
        }
    }

    /// Moves every record rustls has queued into `wire`, after what it
    /// holds already, and sends them. The caller holds the send lock.
    async fn send_queued(&self, wire: &mut Vec<u8>) -> io::Result<()> {
        loop {
            {
                let mut conn = self.conn();
                while conn.wants_write() {
                    conn.write_tls(wire)?;
                }
            }
            if wire.is_empty() {
                return Ok(());
            }
            self.send_wire(wire).await?;
        }
    }

    /// `true` if records wait to be sent: queued by rustls, or left unsent
    /// by a receive.
    fn wants_send(&self) -> bool {
        self.conn().wants_write() || !self.unsent.borrow().is_empty()
    }

    /// Sends the records rustls has queued without waiting, for a receive on
    /// an established stream: a receive may be dropped at any `.await`, and a
    /// send dropped half way would cut a record in two and end the stream's
    /// sending for good. One `send(2)` that does not block has no half way:
    /// what the socket takes is sent, and what it does not take stays in
    /// `unsent`, in order, for the next send or the next receive to go on
    /// from. The peer waits for none of these records (key updates, session
    /// tickets, an alert), so they may be late.
    ///
    /// Does nothing while a task is writing: it sends queued records with
    /// its own data, and `unsent` is empty while it holds the lock.
    fn flush_now(&self) -> io::Result<()> {
        if self.broken.get() {
            return Err(broken());
        }
        if self.sending.get() {
            return Ok(());
        }
        let mut unsent = self.unsent.borrow_mut();
        {
            let mut conn = self.conn();
            while conn.wants_write() {
                conn.write_tls(&mut *unsent)?;
            }
        }
        loop {
            // All of it, or as much as a test lets the socket take.
            let len = unsent.len().min(flush_room());
            if len == 0 {
                return Ok(());
            }
            // SAFETY: `unsent` is valid for `len` bytes for the call.
            let n = unsafe {
                libc::send(
                    std::os::fd::AsRawFd::as_raw_fd(&self.tcp),
                    unsent.as_ptr().cast(),
                    len,
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if n >= 0 {
                if n as usize == unsent.len() {
                    // All sent: an idle connection keeps no buffer for this.
                    *unsent = Vec::new();
                } else {
                    // What the socket did not take waits in `unsent`.
                    unsent.drain(..n as usize);
                }
                return Ok(());
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock => return Ok(()),
                _ => {
                    self.broken.set(true);
                    *unsent = Vec::new();
                    return Err(e);
                }
            }
        }
    }

    /// Writes all of `wire` to the socket and empties it, keeping its
    /// capacity for the rest of this write. A failure may have cut a record
    /// in two, so it marks the stream broken.
    async fn send_wire(&self, wire: &mut Vec<u8>) -> io::Result<()> {
        let (res, mut sent) = self.tcp.write_all(mem::take(wire)).await;
        sent.clear();
        *wire = sent;
        if res.is_err() {
            self.broken.set(true);
        }
        res
    }

    /// Encrypts all of `data` and sends it, in the process. The caller holds
    /// the send lock.
    ///
    /// rustls takes up to its buffer limit (64 KiB) per call. Records a
    /// receive queued in the meantime (a key update, an alert) go out first.
    ///
    /// With `last` false, the records of the end of `data` may stay in
    /// `wire` for the next piece of the same write to join them, so that a
    /// response's head and body share one send; the caller passes `last`
    /// true for the final piece. Records the caller found queued (`carried`)
    /// are followed by a send in the same call.
    async fn send_plain(&self, wire: &mut Vec<u8>, data: &[u8], last: bool) -> io::Result<()> {
        let mut sent = 0;
        while sent < data.len() {
            let (carried, refused) = {
                let mut conn = self.conn();
                while conn.wants_write() {
                    conn.write_tls(wire)?;
                }
                let carried = !wire.is_empty();
                let start = wire.len();
                let n = conn.writer().write(&data[sent..])?;
                while conn.wants_write() {
                    conn.write_tls(wire)?;
                }
                sent += n;
                // rustls took the bytes and encrypted none of them: the key
                // has encrypted as many records as its cipher allows and,
                // on TLS 1.2, cannot be changed. rustls queued close_notify
                // instead and drops everything it is given from then on.
                (carried, n > 0 && !has_application_data(&wire[start..]))
            };
            let more = sent < data.len() || !last;
            if !wire.is_empty() && (carried || refused || !more || wire.len() >= SEND_AT) {
                self.send_wire(wire).await?;
            }
            if carried {
                // Receivers waiting in `flush_tls` for these records.
                self.wake_waiters();
            }
            if refused {
                self.closed.set(Some(Code::TlsKeysExhausted));
                return Err(sending_closed(Code::TlsKeysExhausted));
            }
        }
        Ok(())
    }

    /// One write in the process: `pieces` encrypted and sent in order,
    /// holding the send lock throughout, so that no other task's write comes
    /// between them.
    async fn write_in_process(&self, pieces: &[&[u8]]) -> io::Result<()> {
        let sending = self.lock_send().await;
        if let Some(e) = self.cannot_send() {
            sending.done();
            return Err(e);
        }
        let mut wire = self.take_wire();
        let mut res = Ok(());
        for (i, piece) in pieces.iter().enumerate() {
            res = self
                .send_plain(&mut wire, piece, i + 1 == pieces.len())
                .await;
            if res.is_err() {
                break;
            }
        }
        if res.is_ok() && !wire.is_empty() {
            // Records of pieces that came before empty last ones.
            res = self.send_wire(&mut wire).await;
        }
        self.keep_wire(wire);
        // A failure on the socket has marked the stream broken already; one
        // before anything was sent (rustls refusing the data, keys used up,
        // a file read) leaves it whole.
        sending.done();
        res
    }

    /// Receives one chunk of TLS data, decrypts it into `plain`, and sends
    /// whatever the protocol wants to send in response.
    async fn pump(&self) -> io::Result<()> {
        // Records held back for the next send (see `hold_output`) go out
        // now if the receive has to wait: the peer may be waiting for them.
        let ready = if !self.broken.get() && self.wants_send() {
            poll_fn(|cx| Poll::Ready(self.tcp.poll_recv(cx))).await
        } else {
            Poll::Pending
        };
        let chunk = match ready {
            Poll::Ready(chunk) => chunk?,
            Poll::Pending => {
                if !self.broken.get() && self.wants_send() {
                    // A failure marks the stream broken, for the next write
                    // to report; it is no reason to withhold what arrives
                    // (the peer may have sent its last data and gone).
                    let _ = self.flush_for_recv().await;
                }
                // Cancelling here loses nothing: `recv` is cancel safe.
                self.tcp.recv().await?
            }
        };
        self.absorb(chunk).await
    }

    /// Decrypts a received `chunk` into `plain`, and sends whatever the
    /// protocol wants to send in response (unless it may wait, see
    /// [`hold_output`](Self::hold_output)).
    async fn absorb(&self, chunk: giong::RecvBuf) -> io::Result<()> {
        if chunk.is_empty() {
            self.eof.set(true);
            return Ok(());
        }
        let result = {
            let mut conn = self.conn();
            let mut plain = self.plain.borrow_mut();
            let mut input = &chunk[..];
            let mut result = Ok(());
            while !input.is_empty() {
                // rustls buffers a bounded amount of plaintext, so it has to
                // be drained as the records are fed in.
                match conn.read_tls(&mut input) {
                    Ok(0) => {
                        result = Err(coded(
                            Code::TlsProtocol,
                            "rustls accepted no more TLS data",
                            "this is a bug in giong-tls; please report it at \
                             https://github.com/giong-run/Giong/issues.",
                        ));
                        break;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        result = Err(e);
                        break;
                    }
                }
                match conn.process_new_packets() {
                    Ok(state) => {
                        let n = state.plaintext_bytes_to_read();
                        if n > 0 {
                            let at = plain.len();
                            plain.resize(at + n, 0);
                            if let Err(e) = conn.reader().read_exact(&mut plain[at..]) {
                                plain.truncate(at);
                                result = Err(e);
                                break;
                            }
                        }
                        if state.peer_has_closed() {
                            self.close_notify.set(true);
                            self.eof.set(true);
                            break;
                        }
                    }
                    Err(e) => {
                        result = Err(tls_error(e));
                        break;
                    }
                }
            }
            result
        };
        drop(chunk);
        // Handshake messages, key updates, or the alert describing `result`.
        // Not being able to send them is no reason to withhold what was
        // received, if sending has failed before.
        if self.broken.get() || (result.is_ok() && self.hold_output()) {
            return result;
        }
        let flushed = self.flush_for_recv().await;
        result.and(flushed)
    }

    /// Sends queued records on behalf of a receive. During the handshake
    /// the peer waits for them and the future is the handshake's own, so
    /// they are sent in full. Afterwards the receive may be dropped at any
    /// moment and must leave the stream able to send: see
    /// [`flush_now`](Self::flush_now).
    async fn flush_for_recv(&self) -> io::Result<()> {
        if self.established.get() {
            self.flush_now()
        } else {
            self.flush_tls().await
        }
    }

    /// Receives decrypted data. An empty buffer means that the peer has closed
    /// the connection, with a `close_notify` alert or by closing the socket.
    ///
    /// The two are not the same when the end of the stream is what marks
    /// the end of the data, as in an HTTP response without a length: only
    /// `close_notify` is authenticated, while anyone on the network path can
    /// close a TCP connection early and cut the data short (a truncation
    /// attack). Check [`received_close_notify`](Self::received_close_notify)
    /// before trusting such data to be complete.
    ///
    /// Cancel safe: dropping the future loses no data and leaves the stream
    /// able to send. A TLS protocol message the receive owes the peer (a key
    /// update, session tickets) is sent without waiting, and what the socket
    /// does not take at once goes out with the next send or receive.
    ///
    /// One task may receive while others send. Only one task may receive at
    /// a time: like [`TcpStream::recv`], the stream wakes only the task that
    /// waited last, so a second receiver can wait forever.
    pub async fn recv(&self) -> io::Result<Received> {
        if self.is_ktls() {
            return self.kernel_recv().await.map(Received);
        }
        self.user_recv().await.map(Received)
    }

    async fn user_recv(&self) -> io::Result<Vec<u8>> {
        loop {
            {
                let mut plain = self.plain.borrow_mut();
                if !plain.is_empty() {
                    return Ok(mem::take(&mut *plain));
                }
            }
            if let Some(e) = self.failed.take() {
                return Err(e);
            }
            if self.eof.get() {
                return Ok(Vec::new());
            }
            if let Err(e) = self.pump().await {
                // Data that arrived intact before the failure comes first.
                if self.plain.borrow().is_empty() {
                    return Err(e);
                }
                self.failed.set(Some(e));
            }
        }
    }

    /// Encrypts and sends all of `buf`, then hands the buffer back.
    ///
    /// Takes any buffer giong can send, as [`TcpStream::write_all`] does:
    /// a `Vec<u8>`, a `Box<[u8]>`, an `Rc<[u8]>`, a `&'static [u8]`, ...
    ///
    /// Each write goes out whole: another task's `write_all`,
    /// [`write_all_vectored`](Self::write_all_vectored) or
    /// [`send_file`](Self::send_file) on the same stream waits for it, and
    /// their data never interleave.
    ///
    /// Fails with `TlsKeysExhausted` (`GIONG-E2012`) once the connection has
    /// sent as many records as its cipher allows under one key and cannot
    /// change keys (TLS 1.2 with AES-GCM after 2^24 records: rustls sends
    /// `close_notify`, and nothing more can be sent); with `TlsBroken`
    /// (`GIONG-E2008`) after [`shutdown`](Self::shutdown) or a failed write.
    ///
    /// Not cancel safe: if the future is dropped, part of `buf` may have been
    /// sent, and if that happened in the middle of a TLS record the
    /// connection cannot send any more: later writes return an error, while
    /// data the peer already sent can still be received.
    pub async fn write_all<B>(&self, buf: B) -> (io::Result<()>, B)
    where
        B: giong::IoBuf + AsRef<[u8]>,
    {
        if self.is_ktls() {
            return self.kernel_write_all(buf).await;
        }
        let res = self.write_in_process(&[buf.as_ref()]).await;
        (res, buf)
    }

    /// Encrypts and sends all of `bufs`, one after the other, without
    /// copying them into one buffer first, then hands them back.
    ///
    /// Use it to send a header and a large body that live in different
    /// buffers. In the process, rustls takes the pieces one by one, so a
    /// small header goes out in the same write as the start of the body;
    /// with kernel TLS they go to the kernel in vectored writes, as with
    /// [`TcpStream::write_all_vectored`]. All of them go out as one write,
    /// as with [`write_all`](Self::write_all), which also has the errors.
    ///
    /// Not cancel safe, as [`write_all`](Self::write_all).
    pub async fn write_all_vectored<B>(&self, bufs: Vec<B>) -> (io::Result<()>, Vec<B>)
    where
        B: giong::IoBuf + AsRef<[u8]>,
    {
        if self.is_ktls() {
            return self.kernel_write_all_vectored(bufs).await;
        }
        let pieces: Vec<&[u8]> = bufs.iter().map(AsRef::as_ref).collect();
        let res = self.write_in_process(&pieces).await;
        drop(pieces);
        (res, bufs)
    }

    /// Encrypts and sends `len` bytes of `file`, starting at `offset`.
    ///
    /// With kernel TLS (see [`is_ktls`](Self::is_ktls)) this is
    /// [`TcpStream::send_file`]: the kernel encrypts the file straight from
    /// its page cache, and the bytes never pass through the program. This is
    /// where kernel TLS is faster than encrypting in the process. Without
    /// it, the file is read in pieces of up to 256 KiB and encrypted by
    /// rustls. Either way, the file's own position is neither used nor
    /// moved.
    ///
    /// Fails with `FileTooShort` (`GIONG-E0201`) if the file ends before
    /// `offset + len`, after sending what it had; the stream can still send.
    /// The notes of [`TcpStream::send_file`] on cold files apply to kernel
    /// TLS.
    ///
    /// Not cancel safe, as [`write_all`](Self::write_all).
    pub async fn send_file(&self, file: &giong::fs::File, offset: u64, len: u64) -> io::Result<()> {
        if offset
            .checked_add(len)
            .map_or(true, |end| end > i64::MAX as u64)
        {
            return Err(file_too_short(offset, len, offset));
        }
        if self.is_ktls() {
            return self.kernel_send_file(file, offset, len).await;
        }
        let sending = self.lock_send().await;
        if let Some(e) = self.cannot_send() {
            sending.done();
            return Err(e);
        }
        let mut wire = self.take_wire();
        let res = self
            .send_file_in_process(&mut wire, file, offset, len)
            .await;
        self.keep_wire(wire);
        sending.done();
        res
    }

    /// `send_file` in the process: read and encrypt the file in pieces. The
    /// caller holds the send lock.
    async fn send_file_in_process(
        &self,
        wire: &mut Vec<u8>,
        file: &giong::fs::File,
        offset: u64,
        len: u64,
    ) -> io::Result<()> {
        const PIECE: u64 = 256 * 1024;
        let mut buf = Vec::with_capacity(len.min(PIECE) as usize);
        let (mut at, mut left) = (offset, len);
        while left > 0 {
            let (res, b) = file.read_at(buf, at).await;
            buf = b;
            let n = match res {
                Ok(0) => return Err(file_too_short(offset, len, at)),
                Ok(n) => (n as u64).min(left) as usize,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            self.send_plain(wire, &buf[..n], true).await?;
            at += n as u64;
            left -= n as u64;
        }
        Ok(())
    }

    /// Sends the records `shutdown` queued (`close_notify`, and any held
    /// back before it) so that they share a segment with the FIN that
    /// follows: one `send(2)` with `MSG_MORE` holds them in the socket until
    /// `shutdown(2)` adds the FIN and pushes both. What that call cannot
    /// take at once goes the usual way. The caller holds the send lock, so
    /// no other send is in flight.
    async fn send_closing(&self, wire: &mut Vec<u8>) -> io::Result<()> {
        {
            let mut conn = self.conn();
            while conn.wants_write() {
                conn.write_tls(wire)?;
            }
        }
        // SAFETY: `wire` is valid for `wire.len()` bytes for the call.
        let n = unsafe {
            libc::send(
                std::os::fd::AsRawFd::as_raw_fd(&self.tcp),
                wire.as_ptr().cast(),
                wire.len(),
                libc::MSG_MORE | libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if n >= 0 {
            wire.drain(..n as usize);
        } else {
            let e = io::Error::last_os_error();
            if !matches!(
                e.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) {
                self.broken.set(true);
                wire.clear();
                return Err(e);
            }
        }
        if !wire.is_empty() {
            self.send_wire(wire).await?;
        }
        self.send_queued(wire).await
    }

    /// Sends the TLS `close_notify` alert and closes the sending direction.
    ///
    /// Waits for a write in progress. After that, writes fail with
    /// `TlsBroken` (`GIONG-E2008`). On a stream that can no longer send (a
    /// failed or cancelled write, keys used up, a second `shutdown`), only
    /// the TCP sending direction is closed, with no alert.
    ///
    /// If the peer has closed its socket already, so that the alert cannot
    /// be delivered (`EPIPE`, `ECONNRESET`: a client that sent its own
    /// `close_notify` and left, which the stream may not have read yet),
    /// this returns `Ok`: the sending direction is closed either way, and
    /// nobody is left to tell (Tokio's `TcpStream::shutdown` likewise
    /// returns `Ok` for `ENOTCONN` after a reset). A failure of an earlier
    /// write is still reported by that write.
    ///
    /// Not cancel safe: if the future is dropped, the `close_notify` alert may
    /// have been sent in part, and the stream can send nothing more.
    pub async fn shutdown(&self) -> io::Result<()> {
        if self.is_ktls() {
            return self.kernel_shutdown().await;
        }
        let sending = self.lock_send().await;
        if self.cannot_send().is_none() {
            self.conn().send_close_notify();
            self.closed.set(Some(Code::TlsBroken));
            let mut wire = self.take_wire();
            let res = self.send_closing(&mut wire).await;
            self.keep_wire(wire);
            if let Err(e) = res {
                // Close the TCP side anyway, so the peer is not left waiting.
                sending.done();
                let _ = self.tcp.shutdown(Shutdown::Write);
                return if self.peer_gone(&e) { Ok(()) } else { Err(e) };
            }
        }
        sending.done();
        self.tcp.shutdown(Shutdown::Write)
    }

    /// `true` if `e`, from sending `close_notify`, only says that the peer
    /// has closed its socket.
    fn peer_gone(&self, e: &io::Error) -> bool {
        matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        )
    }

    /// `true` once the peer has ended the connection with a TLS
    /// `close_notify` alert. `false` while it is open, and if the peer, or
    /// something on the network path, closed the TCP connection without
    /// one: data that ends there may have been cut short. See
    /// [`recv`](Self::recv).
    pub fn received_close_notify(&self) -> bool {
        self.close_notify.get()
    }

    /// The protocol agreed on through ALPN, if any (for example `http/1.1`).
    pub fn alpn_protocol(&self) -> Option<Vec<u8>> {
        match &*self.engine.borrow() {
            Engine::User(c) => c.alpn_protocol().map(<[u8]>::to_vec),
            Engine::Kernel(k) => k.negotiated.alpn.clone(),
        }
    }

    /// The certificate chain the peer presented, its own certificate first:
    /// the server's on a client connection; on a server connection, the
    /// client's if client authentication is configured and the client sent
    /// one.
    pub fn peer_certificates(&self) -> Option<Vec<CertificateDer<'static>>> {
        match &*self.engine.borrow() {
            Engine::User(c) => c.peer_certificates().map(<[_]>::to_vec),
            Engine::Kernel(k) => k.negotiated.peer_certificates.clone(),
        }
    }

    /// `true` if the handshake resumed an earlier session instead of doing a
    /// full one.
    pub fn resumed(&self) -> bool {
        match &*self.engine.borrow() {
            Engine::User(c) => c.handshake_kind() == Some(rustls::HandshakeKind::Resumed),
            Engine::Kernel(k) => k.negotiated.resumed,
        }
    }

    /// The underlying TCP connection, for its addresses and socket options
    /// (`peer_addr`, `set_nodelay`, ...).
    ///
    /// Do not send or receive on it: in the process that corrupts the TLS
    /// record stream, and under kernel TLS it bypasses the record limits
    /// and the ordering of writes this type keeps.
    pub fn tcp(&self) -> &TcpStream {
        &self.tcp
    }
}

impl std::fmt::Debug for TlsStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsStream")
            .field("tcp", &self.tcp)
            .field("ktls", &self.is_ktls())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[test]
    fn a_connector_without_trusted_authorities_is_refused() {
        // The same kind as when the machine has none (`with_system_roots`).
        let err = TlsConnector::from_roots(RootCertStore::empty()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert_eq!(
            giong::error::code(&err),
            Some(Code::NoTrustedRoots),
            "{err}"
        );
    }

    fn authority(name: &str) -> (rcgen::Certificate, rcgen::KeyPair) {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        (params.self_signed(&key).unwrap(), key)
    }

    fn signed_by(
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

    /// `with_root_pem_any_name` skips the name and nothing else.
    #[test]
    fn any_name_still_checks_the_chain() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::UnixTime;
        use rustls::CertificateError;

        let (ca, ca_key) = authority("ours");
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(ca.der().to_vec())).unwrap();
        let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            provider(),
        )
        .build()
        .unwrap();
        let verifier = AnyName(webpki);
        let name = ServerName::try_from("db.internal").unwrap();
        let check = |cert: &CertificateDer<'_>| {
            verifier.verify_server_cert(cert, &[], &name, &[], UnixTime::now())
        };
        // Signed by the authority: accepted, whatever names it has.
        for names in [&[][..], &["localhost"], &["db.internal"]] {
            check(&signed_by(&ca, &ca_key, names, false)).unwrap();
        }
        // Expired: refused.
        let e = check(&signed_by(&ca, &ca_key, &[], true)).unwrap_err();
        assert!(
            matches!(
                e,
                rustls::Error::InvalidCertificate(
                    CertificateError::Expired | CertificateError::ExpiredContext { .. }
                )
            ),
            "{e:?}"
        );
        // Another authority's, or self-signed: refused, even with the
        // right name.
        let (other, other_key) = authority("theirs");
        for cert in [
            signed_by(&other, &other_key, &["db.internal"], false),
            CertificateDer::from(other.der().to_vec()),
        ] {
            let e = check(&cert).unwrap_err();
            assert!(matches!(e, rustls::Error::InvalidCertificate(_)), "{e:?}");
        }
        // The authority itself, offered as the server's certificate.
        assert!(check(&CertificateDer::from(ca.der().to_vec())).is_err());
        // PEM that holds no certificate, or a broken one.
        assert!(TlsConnector::with_root_pem_any_name(b"").is_err());
        assert!(TlsConnector::with_root_pem_any_name(
            b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n"
        )
        .is_err());
        assert!(TlsConnector::with_root_pem_any_name(ca.pem().as_bytes()).is_ok());
    }

    /// The start of a ClientHello record listing `suites`, with a session
    /// id of `id_len` bytes, cut after `keep` bytes.
    fn client_hello(suites: &[u16], id_len: u8, keep: usize) -> Vec<u8> {
        let mut hello = vec![22, 3, 1, 0, 0, 1, 0, 0, 0, 3, 3];
        hello.extend_from_slice(&[7; 32]);
        hello.push(id_len);
        hello.extend(std::iter::repeat(9).take(id_len.into()));
        hello.extend_from_slice(&((suites.len() * 2) as u16).to_be_bytes());
        for suite in suites {
            hello.extend_from_slice(&suite.to_be_bytes());
        }
        hello.extend_from_slice(&[1, 0]); // no compression
        hello.truncate(keep);
        hello
    }

    #[test]
    fn chacha20_first_clients_are_told_apart() {
        let all = usize::MAX;
        // OpenSSL: AES-256-GCM, ChaCha20, AES-128-GCM.
        assert!(!prefers_chacha(&client_hello(
            &[0x1302, 0x1303, 0x1301],
            32,
            all
        )));
        // Browsers: AES-128-GCM first.
        assert!(!prefers_chacha(&client_hello(
            &[0x1301, 0x1302, 0x1303],
            0,
            all
        )));
        // A phone without AES instructions, after a GREASE value.
        assert!(prefers_chacha(&client_hello(
            &[0x0a0a, 0x1303, 0x1301],
            32,
            all
        )));
        // TLS 1.2 only.
        assert!(prefers_chacha(&client_hello(&[0xcca9, 0xc02b], 0, all)));
        assert!(!prefers_chacha(&client_hello(&[0xc02c, 0xcca9], 0, all)));
        // Neither, cut short, or not a ClientHello: the server's order.
        assert!(!prefers_chacha(&client_hello(&[0x009c], 0, all)));
        let cut = client_hello(&[0x1303, 0x1301], 32, all).len() - 3;
        assert!(!prefers_chacha(&client_hello(&[0x1303, 0x1301], 32, cut)));
        assert!(!prefers_chacha(&client_hello(&[0x1303], 32, 40)));
        assert!(!prefers_chacha(&[]));
        let mut alert = client_hello(&[0x1303], 0, all);
        alert[0] = 21;
        assert!(!prefers_chacha(&alert));
    }

    fn ca_pem(name: &str) -> String {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.self_signed(&key).unwrap().pem()
    }

    /// A fresh, empty directory under the system's temporary directory.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("giong-tls-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn os(path: &Path) -> &OsStr {
        path.as_os_str()
    }

    #[test]
    fn server_names_are_dns_names_or_ip_addresses() {
        let dns = |n: &str| matches!(parse_server_name(n), Ok(ServerName::DnsName(_)));
        let ip = |n: &str| matches!(parse_server_name(n), Ok(ServerName::IpAddress(_)));
        assert!(dns("example.com"));
        assert!(dns("db.prod.svc.cluster.local"));
        assert!(ip("10.0.0.1"));
        assert!(ip("::1"));
        assert!(ip("[::1]"));
        for bad in ["", "not a name", "[nope]", "[10.0.0.1]", "a..b"] {
            let err = parse_server_name(bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{bad:?}");
        }
    }

    #[test]
    fn the_first_bundle_that_exists_is_read() {
        let dir = scratch("bundle");
        let bundle = dir.join("bundle.pem");
        let two = format!("{}{}", ca_pem("one"), ca_pem("two"));
        std::fs::write(&bundle, two).unwrap();
        let missing = dir.join("missing.pem");
        let bundles = [missing.to_str().unwrap(), bundle.to_str().unwrap()];
        let roots = load_roots(None, None, &bundles, "/nonexistent").unwrap();
        assert_eq!(roots.len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ssl_cert_file_and_dir_replace_the_system_locations() {
        let dir = scratch("env");
        let file = dir.join("file.pem");
        std::fs::write(&file, ca_pem("from file")).unwrap();
        let certs = dir.join("certs");
        std::fs::create_dir(&certs).unwrap();
        std::fs::write(certs.join("a.pem"), ca_pem("a")).unwrap();
        std::fs::write(certs.join("b.pem"), ca_pem("b")).unwrap();
        std::fs::write(certs.join("README"), "not a certificate").unwrap();
        // OpenSSL's hashed names are links to the same files.
        std::os::unix::fs::symlink(certs.join("a.pem"), certs.join("1a2b3c4d.0")).unwrap();
        let system = dir.join("system.pem");
        std::fs::write(&system, ca_pem("system")).unwrap();
        let bundles = [system.to_str().unwrap()];

        let roots = load_roots(Some(os(&file)), None, &bundles, "/nonexistent").unwrap();
        assert_eq!(roots.len(), 1, "SSL_CERT_FILE only");
        let roots = load_roots(None, Some(os(&certs)), &bundles, "/nonexistent").unwrap();
        assert_eq!(roots.len(), 2, "SSL_CERT_DIR only, each file once");
        let both = std::env::join_paths([dir.join("nowhere"), certs.clone()]).unwrap();
        let roots = load_roots(Some(os(&file)), Some(&both), &bundles, "/nonexistent").unwrap();
        assert_eq!(roots.len(), 3, "both, with a missing directory in the list");

        let err = load_roots(Some(os(&dir.join("gone.pem"))), None, &bundles, "").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("SSL_CERT_FILE"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn without_a_bundle_the_directory_is_read_and_nothing_is_an_error() {
        let dir = scratch("dir");
        std::fs::write(dir.join("only.crt"), ca_pem("only")).unwrap();
        let roots = load_roots(
            None,
            None,
            &["/nonexistent/bundle.crt"],
            dir.to_str().unwrap(),
        );
        assert_eq!(roots.unwrap().len(), 1);

        let empty = scratch("empty");
        let err = load_roots(None, None, &[], empty.to_str().unwrap()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("webpki-roots"), "{err}");
        std::fs::remove_dir_all(dir).unwrap();
        std::fs::remove_dir_all(empty).unwrap();
    }

    #[test]
    fn records_are_scanned_for_application_data() {
        // Header: type, version, length; then that many bytes.
        let record = |typ: u8, len: u16| {
            let mut r = vec![typ, 3, 3];
            r.extend_from_slice(&len.to_be_bytes());
            r.resize(5 + len as usize, 0xAA);
            r
        };
        assert!(!has_application_data(&[]));
        assert!(!has_application_data(&record(21, 26)));
        assert!(has_application_data(&record(23, 0)));
        let mut alert_then_data = record(21, 26);
        alert_then_data.extend(record(23, 300));
        assert!(has_application_data(&alert_then_data));
        // A data record's body never counts as a header.
        let mut alert = record(21, 26);
        alert[5] = 23;
        assert!(!has_application_data(&alert));
    }

    #[test]
    fn secrets_are_extractable_only_when_the_kernel_will_take_them() {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let (cert, key) = (key.cert.pem(), key.key_pair.serialize_pem());
        for mode in [Ktls::Off, Ktls::Auto] {
            let acceptor = TlsAcceptor::with_certificates(
                &Certificates::from_pem(cert.as_bytes(), key.as_bytes()).unwrap(),
            )
            .unwrap()
            .ktls(mode)
            .unwrap();
            let tries = acceptor.ktls.tries();
            assert_eq!(acceptor.config.enable_secret_extraction, tries, "{mode:?}");
            let connector = TlsConnector::with_root_pem(cert.as_bytes())
                .unwrap()
                .ktls(mode)
                .unwrap();
            let tries = connector.ktls.tries();
            assert_eq!(connector.config.enable_secret_extraction, tries, "{mode:?}");
        }
    }

    #[test]
    fn this_machines_roots_load() {
        // Every machine the tests run on (CI included) has a CA bundle.
        TlsConnector::with_system_roots().unwrap();
    }

    #[cfg(feature = "webpki-roots")]
    #[test]
    fn mozillas_roots_are_compiled_in() {
        TlsConnector::with_webpki_roots();
    }

    #[test]
    fn records_a_receive_could_not_send_whole_go_out_first_and_in_order() {
        // The socket takes one byte per attempt, so the session tickets a
        // TLS 1.3 server owes after its handshake leave in pieces, each sent
        // by a receive that is then dropped. The write that follows has to
        // send the rest of them before its own record; a byte lost, repeated
        // or out of place fails the client's decryption.
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = key.cert.pem();
        let acceptor = TlsAcceptor::with_certificates(
            &Certificates::from_pem(cert.as_bytes(), key.key_pair.serialize_pem().as_bytes())
                .unwrap(),
        )
        .unwrap();
        let connector = TlsConnector::with_root_pem(cert.as_bytes()).unwrap();
        giong::block_on(async move {
            let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = giong::spawn(async move {
                let tcp = listener.accept().await.unwrap();
                let tls = acceptor.accept(tcp).await.unwrap();
                assert!(tls.conn().wants_write(), "tickets wait for the first send");
                FLUSH_ROOM.set(1);
                let mut left = None;
                for _ in 0..5 {
                    let mut recv = std::pin::pin!(tls.recv());
                    let ready = poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx).is_ready())).await;
                    assert!(!ready);
                    let unsent = tls.unsent.borrow().len();
                    assert!(
                        left.map_or(unsent > 0, |l| unsent + 1 == l),
                        "{left:?} {unsent}"
                    );
                    left = Some(unsent);
                }
                FLUSH_ROOM.set(usize::MAX);
                tls.write_all(&b"after the tickets"[..]).await.0.unwrap();
                // Sent, and the buffer given back: an idle connection keeps
                // none for this.
                assert_eq!(tls.unsent.borrow().capacity(), 0);
                tls.shutdown().await.unwrap();
            });
            let tcp = TcpStream::connect(addr).await.unwrap();
            let tls = connector.connect("localhost", tcp).await.unwrap();
            let mut got = Vec::new();
            loop {
                let data = tls.recv().await.unwrap();
                if data.is_empty() {
                    break;
                }
                got.extend_from_slice(&data);
            }
            assert_eq!(got, b"after the tickets");
            assert!(tls.received_close_notify());
            server.await.unwrap();
        });
    }

    #[test]
    fn a_receive_that_sent_what_was_owed_keeps_no_buffer_for_it() {
        // A TLS 1.3 server's first receive sends its session tickets. The
        // buffer they pass through is given back once they are sent: 2,000
        // idle connections held about 200 bytes each when it was kept.
        let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let cert = key.cert.pem();
        let acceptor = TlsAcceptor::with_certificates(
            &Certificates::from_pem(cert.as_bytes(), key.key_pair.serialize_pem().as_bytes())
                .unwrap(),
        )
        .unwrap();
        let connector = TlsConnector::with_root_pem(cert.as_bytes()).unwrap();
        giong::block_on(async move {
            let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = giong::spawn(async move {
                let tcp = listener.accept().await.unwrap();
                let tls = acceptor.accept(tcp).await.unwrap();
                assert!(tls.conn().wants_write(), "tickets wait for the first send");
                // The client sends nothing, so this receive has to wait,
                // and sends them first.
                let mut recv = std::pin::pin!(tls.recv());
                let ready = poll_fn(|cx| Poll::Ready(recv.as_mut().poll(cx).is_ready())).await;
                assert!(!ready);
                assert!(!tls.conn().wants_write());
                assert_eq!(tls.unsent.borrow().capacity(), 0);
                tls.shutdown().await.unwrap();
            });
            let tcp = TcpStream::connect(addr).await.unwrap();
            let tls = connector.connect("localhost", tcp).await.unwrap();
            assert!(tls.recv().await.unwrap().is_empty());
            assert!(tls.received_close_notify());
            server.await.unwrap();
        });
    }

    #[test]
    fn the_key_exchange_offers_post_quantum_first_with_the_feature() {
        let first = provider().kx_groups[0].name();
        let pq = rustls::NamedGroup::X25519MLKEM768;
        assert_eq!(first == pq, cfg!(feature = "post-quantum"), "{first:?}");
        assert_eq!(provider_aes128_first().kx_groups[0].name(), first);
    }
}
