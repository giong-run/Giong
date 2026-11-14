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
//! use giong_tls::TlsAcceptor;
//!
//! # fn main() -> std::io::Result<()> {
//! let cert = std::fs::read("cert.pem")?;
//! let key = std::fs::read("key.pem")?;
//! let acceptor = TlsAcceptor::from_pem(&cert, &key)?;
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

mod ktls;

pub use ktls::{ktls_stats, ktls_support, Ktls, KtlsStats, KtlsSupport};

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
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::sign::CertifiedKey;
use rustls::{
    ClientConfig, ClientConnection, Connection, RootCertStore, ServerConfig, ServerConnection,
    SignatureScheme,
};

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
fn bad_certificate_data(e: impl std::fmt::Display) -> io::Error {
    coded(
        Code::BadCertificateData,
        e.to_string(),
        "check that the file or data is PEM (\"-----BEGIN CERTIFICATE-----\") and holds \
         at least one certificate.",
    )
}

/// A private key that cannot be read.
fn bad_key(e: impl std::fmt::Display) -> io::Error {
    coded(
        Code::BadCertificateData,
        format!("cannot read the private key: {e}"),
        "check that it is a PEM private key (\"-----BEGIN PRIVATE KEY-----\", or the RSA or \
         EC forms).",
    )
}

/// A certificate and a key that cannot be used together.
fn mismatched_key(e: rustls::Error) -> io::Error {
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
        rustls::Error::InvalidCertificate(_) => coded(
            Code::TlsCertificate,
            format!("the peer's certificate was rejected: {e}"),
            "check the name you connect to, the certificate's expiry and its chain; a \
             private authority has to be trusted with TlsConnector::with_roots.",
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

fn broken() -> io::Error {
    coded(
        Code::TlsBroken,
        "a TLS write failed or was cancelled; nothing more can be sent",
        "open a new connection. A write cancelled half-way (a dropped future, a timeout) \
         leaves a partial record or message behind, so the stream cannot go on.",
    )
}

/// Every certificate in `pem`, or an error if there is none.
fn pem_certs(pem: &[u8]) -> io::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(bad_certificate_data)?;
    if certs.is_empty() {
        return Err(bad_certificate_data("no certificate found in the PEM data"));
    }
    Ok(certs)
}

fn ring() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Turns accepted TCP connections into TLS connections.
///
/// Cheap to clone; share one between all workers.
#[derive(Clone)]
pub struct TlsAcceptor {
    config: Arc<ServerConfig>,
    handshake_timeout: Duration,
    ktls: ktls::Plan,
}

impl TlsAcceptor {
    /// An acceptor using an existing rustls configuration.
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self {
            config,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ktls: ktls::Plan::Off,
        }
    }

    /// An acceptor for one certificate chain and its private key, both in PEM
    /// format. TLS 1.2 and 1.3 with rustls' default cipher suites, no client
    /// authentication, and `http/1.1` offered through ALPN.
    pub fn from_pem(cert_chain: &[u8], private_key: &[u8]) -> io::Result<Self> {
        let certs = pem_certs(cert_chain)?;
        let key = PrivateKeyDer::from_pem_slice(private_key).map_err(bad_key)?;
        let mut config = ServerConfig::builder_with_provider(ring())
            .with_safe_default_protocol_versions()
            .map_err(tls_error)?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(mismatched_key)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self::new(Arc::new(config)))
    }

    /// The protocols to accept through ALPN, most preferred first, for
    /// example `&[b"h2", b"http/1.1"]`. A client that offers none of them
    /// fails the handshake; one that offers no ALPN at all is accepted.
    /// The protocol agreed on is [`TlsStream::alpn_protocol`].
    ///
    /// giong-http's `Server::serve_tls` sets this itself, to the protocols
    /// the server speaks.
    pub fn alpn(mut self, protocols: &[&[u8]]) -> Self {
        Arc::make_mut(&mut self.config).alpn_protocols =
            protocols.iter().map(|p| p.to_vec()).collect();
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
    /// acceptor's copy of the configuration.
    ///
    /// ```no_run
    /// use giong_tls::{Ktls, TlsAcceptor};
    ///
    /// # fn main() -> std::io::Result<()> {
    /// let acceptor = TlsAcceptor::from_pem(&std::fs::read("cert.pem")?, &std::fs::read("key.pem")?)?
    ///     .ktls(Ktls::Auto)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn ktls(mut self, mode: Ktls) -> io::Result<Self> {
        let early_data = self.config.max_early_data_size > 0;
        self.ktls = ktls::Plan::new(mode, self.config.crypto_provider(), early_data)?;
        if mode != Ktls::Off {
            Arc::make_mut(&mut self.config).enable_secret_extraction = true;
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
    pub async fn accept(&self, tcp: TcpStream) -> io::Result<TlsStream> {
        let tcp = match &self.ktls {
            ktls::Plan::Off => tcp,
            ktls::Plan::Try { required } => {
                let start = ktls::Start::Server(self.config.clone());
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
        let conn = ServerConnection::new(self.config.clone()).map_err(tls_error)?;
        TlsStream::new(tcp, conn.into())
            .handshake(self.handshake_timeout)
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
    /// A connector using an existing rustls configuration.
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self {
            config,
            handshake_timeout: HANDSHAKE_TIMEOUT,
            ktls: ktls::Plan::Off,
        }
    }

    /// A connector that trusts the given certificate authorities. TLS 1.2
    /// and 1.3 with rustls' default cipher suites.
    pub fn with_roots(roots: RootCertStore) -> io::Result<Self> {
        if roots.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                giong::error::Error::new(
                    Code::NoTrustedRoots,
                    "no trusted certificate authority given",
                    "pass at least one certificate, or use TlsConnector::with_system_roots.",
                ),
            ));
        }
        let config = ClientConfig::builder_with_provider(ring())
            .with_safe_default_protocol_versions()
            .map_err(tls_error)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self::new(Arc::new(config)))
    }

    /// A connector that trusts the certificate authorities in `pem`: a
    /// private CA, or a server's own self-signed certificate.
    pub fn with_root_pem(pem: &[u8]) -> io::Result<Self> {
        let mut roots = RootCertStore::empty();
        for cert in pem_certs(pem)? {
            roots.add(cert).map_err(bad_certificate_data)?;
        }
        Self::with_roots(roots)
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
        Self::with_roots(roots)
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
        Self::with_roots(roots).expect("the webpki-roots crate provides roots")
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
        if mode != Ktls::Off {
            Arc::make_mut(&mut self.config).enable_secret_extraction = true;
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
            .handshake(self.handshake_timeout)
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
    /// Scratch buffer for outgoing TLS records, reused between writes.
    wire: RefCell<Vec<u8>>,
    /// The peer closed the connection (with or without `close_notify`).
    eof: Cell<bool>,
    /// The peer ended its side with a `close_notify` alert, so nothing it
    /// sent was cut off.
    close_notify: Cell<bool>,
    /// A task is writing TLS records to the socket. Records must not
    /// interleave, so other tasks wait for it in `waiters`.
    sending: Cell<bool>,
    /// A write to the socket failed or was cancelled half way: the record
    /// stream is cut in the middle and nothing more can be sent.
    broken: Cell<bool>,
    /// A receive error held back until the data before it has been handed out.
    failed: Cell<Option<io::Error>>,
    /// Tasks waiting for `sending` to clear.
    waiters: RefCell<Vec<Waker>>,
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
        let waiters = mem::take(&mut *self.stream.waiters.borrow_mut());
        for waker in waiters {
            waker.wake();
        }
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
            eof: Cell::new(peer_closed),
            close_notify: Cell::new(peer_closed),
            sending: Cell::new(false),
            broken: Cell::new(false),
            failed: Cell::new(None),
            waiters: RefCell::new(Vec::new()),
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

    /// Runs the handshake, either side, within `limit`.
    async fn handshake(self, limit: Duration) -> io::Result<Self> {
        let handshake = async {
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
            self.flush_tls().await
        };
        match timeout(limit, handshake).await {
            Ok(Ok(())) => Ok(self),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(handshake_timed_out(limit)),
        }
    }

    /// Sends every TLS record rustls has queued.
    async fn flush_tls(&self) -> io::Result<()> {
        loop {
            if !self.conn().wants_write() {
                return Ok(());
            }
            if self.broken.get() {
                return Err(broken());
            }
            if self.sending.get() {
                // Another task is sending. Wait for it, then send what is
                // left (it may already have sent this task's records too).
                self.wait_for_sender().await;
                continue;
            }
            let mut wire = mem::take(&mut *self.wire.borrow_mut());
            {
                let mut conn = self.conn();
                while conn.wants_write() {
                    conn.write_tls(&mut wire)?;
                }
            }
            let sending = self.lock_send().await;
            let (res, mut wire) = self.tcp.write_all(wire).await;
            res?;
            sending.done();
            wire.clear();
            *self.wire.borrow_mut() = wire;
        }
    }

    /// Receives one chunk of TLS data, decrypts it into `plain`, and sends
    /// whatever the protocol wants to send in response.
    async fn pump(&self) -> io::Result<()> {
        // Cancelling here loses nothing: `recv` is cancel safe.
        let chunk = self.tcp.recv().await?;
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
        if self.broken.get() {
            return result;
        }
        let flushed = self.flush_tls().await;
        result.and(flushed)
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
    /// Cancel safe: dropping the future loses no data. (If it is dropped at
    /// the rare moment a TLS protocol message is being sent, sending fails
    /// from then on instead of continuing on a broken stream.)
    ///
    /// One task may receive while another one sends.
    pub async fn recv(&self) -> io::Result<Vec<u8>> {
        if self.is_ktls() {
            return self.kernel_recv().await;
        }
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
    /// Not cancel safe: if the future is dropped, part of `buf` may have been
    /// sent, and if that happened in the middle of a TLS record the
    /// connection cannot send any more: later writes return an error, while
    /// data the peer already sent can still be received.
    pub async fn write_all(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        if self.is_ktls() {
            return self.kernel_write_all(buf).await;
        }
        let mut sent = 0;
        while sent < buf.len() {
            // rustls accepts a bounded amount per call; send as it fills up.
            let accepted = self.conn().writer().write(&buf[sent..]);
            match accepted {
                Ok(n) => sent += n,
                Err(e) => return (Err(e), buf),
            }
            if let Err(e) = self.flush_tls().await {
                return (Err(e), buf);
            }
        }
        (Ok(()), buf)
    }

    /// Encrypts and sends all of `bufs`, one after the other, without
    /// copying them into one buffer first, then hands them back.
    ///
    /// Use it to send a header and a large body that live in different
    /// buffers. In the process, rustls takes the pieces one by one, so a
    /// small header goes out in the same write as the start of the body;
    /// with kernel TLS they go to the kernel in vectored writes, as with
    /// [`TcpStream::write_all_vectored`].
    ///
    /// Not cancel safe, as [`write_all`](Self::write_all).
    pub async fn write_all_vectored<B>(&self, bufs: Vec<B>) -> (io::Result<()>, Vec<B>)
    where
        B: giong::IoBuf + AsRef<[u8]>,
    {
        if self.is_ktls() {
            return self.kernel_write_all_vectored(bufs).await;
        }
        for buf in &bufs {
            let buf = buf.as_ref();
            let mut sent = 0;
            while sent < buf.len() {
                let accepted = self.conn().writer().write(&buf[sent..]);
                match accepted {
                    Ok(n) => sent += n,
                    Err(e) => return (Err(e), bufs),
                }
                // rustls takes a bounded amount: send what it holds once it
                // is full, and the rest at the end.
                if sent < buf.len() {
                    if let Err(e) = self.flush_tls().await {
                        return (Err(e), bufs);
                    }
                }
            }
        }
        match self.flush_tls().await {
            Ok(()) => (Ok(()), bufs),
            Err(e) => (Err(e), bufs),
        }
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
        const PIECE: u64 = 256 * 1024;
        let mut buf = Vec::with_capacity(len.min(PIECE) as usize);
        let (mut at, mut left) = (offset, len);
        while left > 0 {
            let (res, b) = file.read_at(buf, at).await;
            buf = b;
            let n = match res {
                Ok(0) => {
                    self.flush_tls().await?;
                    return Err(file_too_short(offset, len, at));
                }
                Ok(n) => (n as u64).min(left) as usize,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let mut sent = 0;
            while sent < n {
                let accepted = self.conn().writer().write(&buf[sent..n]);
                sent += accepted?;
                self.flush_tls().await?;
            }
            at += n as u64;
            left -= n as u64;
        }
        Ok(())
    }

    /// Sends the TLS `close_notify` alert and closes the sending direction.
    pub async fn shutdown(&self) -> io::Result<()> {
        if self.is_ktls() {
            return self.kernel_shutdown().await;
        }
        self.conn().send_close_notify();
        self.flush_tls().await?;
        self.tcp.shutdown(Shutdown::Write)
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

    /// The underlying TCP connection.
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
    fn this_machines_roots_load() {
        // Every machine the tests run on (CI included) has a CA bundle.
        TlsConnector::with_system_roots().unwrap();
    }

    #[cfg(feature = "webpki-roots")]
    #[test]
    fn mozillas_roots_are_compiled_in() {
        TlsConnector::with_webpki_roots();
    }
}
