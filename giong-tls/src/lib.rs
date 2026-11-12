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

#![warn(missing_docs)]

use std::cell::{Cell, RefCell};
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

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// How long a handshake may take unless configured otherwise.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Every certificate in `pem`, or an error if there is none.
fn pem_certs(pem: &[u8]) -> io::Result<Vec<CertificateDer<'static>>> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    if certs.is_empty() {
        return Err(invalid("no certificate found in the PEM data"));
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
}

impl TlsAcceptor {
    /// An acceptor using an existing rustls configuration.
    pub fn new(config: Arc<ServerConfig>) -> Self {
        Self {
            config,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }

    /// An acceptor for one certificate chain and its private key, both in PEM
    /// format. TLS 1.2 and 1.3 with rustls' default cipher suites, no client
    /// authentication, and `http/1.1` offered through ALPN.
    pub fn from_pem(cert_chain: &[u8], private_key: &[u8]) -> io::Result<Self> {
        let certs = pem_certs(cert_chain)?;
        let key = PrivateKeyDer::from_pem_slice(private_key).map_err(invalid)?;
        let mut config = ServerConfig::builder_with_provider(ring())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(invalid)?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self::new(Arc::new(config)))
    }

    /// How long a client may take to complete the TLS handshake (default:
    /// 10 s).
    pub fn handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Performs the TLS handshake on an accepted connection.
    pub async fn accept(&self, tcp: TcpStream) -> io::Result<TlsStream> {
        let conn = ServerConnection::new(self.config.clone()).map_err(invalid)?;
        TlsStream::new(tcp, conn.into())
            .handshake(self.handshake_timeout)
            .await
    }
}

impl std::fmt::Debug for TlsAcceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsAcceptor")
            .field("handshake_timeout", &self.handshake_timeout)
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
}

impl TlsConnector {
    /// A connector using an existing rustls configuration.
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self {
            config,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }

    /// A connector that trusts the given certificate authorities. TLS 1.2
    /// and 1.3 with rustls' default cipher suites.
    pub fn with_roots(roots: RootCertStore) -> io::Result<Self> {
        if roots.is_empty() {
            return Err(invalid("no trusted certificate authority given"));
        }
        let config = ClientConfig::builder_with_provider(ring())
            .with_safe_default_protocol_versions()
            .map_err(invalid)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self::new(Arc::new(config)))
    }

    /// A connector that trusts the certificate authorities in `pem`: a
    /// private CA, or a server's own self-signed certificate.
    pub fn with_root_pem(pem: &[u8]) -> io::Result<Self> {
        let mut roots = RootCertStore::empty();
        for cert in pem_certs(pem)? {
            roots.add(cert).map_err(invalid)?;
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
        let key = PrivateKeyDer::from_pem_slice(private_key).map_err(invalid)?;
        let key =
            CertifiedKey::from_der(certs, key, self.config.crypto_provider()).map_err(invalid)?;
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
        let conn = ClientConnection::new(self.config.clone(), name).map_err(invalid)?;
        TlsStream::new(tcp, conn.into())
            .handshake(self.handshake_timeout)
            .await
    }
}

impl std::fmt::Debug for TlsConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsConnector")
            .field("handshake_timeout", &self.handshake_timeout)
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
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is neither a DNS name nor an IP address"),
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
                io::Error::new(e.kind(), format!("cannot read SSL_CERT_FILE {file:?}: {e}"))
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
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no trusted CA certificates found on this machine: install the ca-certificates \
             package, set SSL_CERT_FILE, or use TlsConnector::with_webpki_roots \
             (the webpki-roots feature of giong-tls)",
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
    conn: RefCell<Connection>,
    /// Decrypted data not handed out yet.
    plain: RefCell<Vec<u8>>,
    /// Scratch buffer for outgoing TLS records, reused between writes.
    wire: RefCell<Vec<u8>>,
    /// The peer closed the connection (with or without `close_notify`).
    eof: Cell<bool>,
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

/// Held while TLS records are being written to the socket.
struct Sending<'a> {
    stream: &'a TlsStream,
    complete: bool,
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
        Self {
            tcp,
            conn: RefCell::new(conn),
            plain: RefCell::new(Vec::new()),
            wire: RefCell::new(Vec::new()),
            eof: Cell::new(false),
            sending: Cell::new(false),
            broken: Cell::new(false),
            failed: Cell::new(None),
            waiters: RefCell::new(Vec::new()),
        }
    }

    /// Runs the handshake, either side, within `limit`.
    async fn handshake(self, limit: Duration) -> io::Result<Self> {
        let handshake = async {
            while self.conn.borrow().is_handshaking() {
                self.flush_tls().await?;
                if !self.conn.borrow().is_handshaking() {
                    break;
                }
                if self.eof.get() {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the peer closed the connection during the TLS handshake",
                    ));
                }
                self.pump().await?;
            }
            self.flush_tls().await
        };
        match timeout(limit, handshake).await {
            Ok(Ok(())) => Ok(self),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS handshake timed out",
            )),
        }
    }

    /// Sends every TLS record rustls has queued.
    async fn flush_tls(&self) -> io::Result<()> {
        loop {
            if !self.conn.borrow().wants_write() {
                return Ok(());
            }
            if self.broken.get() {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "a TLS write failed or was cancelled; nothing more can be sent",
                ));
            }
            if self.sending.get() {
                // Another task is sending. Wait for it, then send what is
                // left (it may already have sent this task's records too).
                poll_fn(|cx| {
                    if !self.sending.get() {
                        return Poll::Ready(());
                    }
                    self.waiters.borrow_mut().push(cx.waker().clone());
                    Poll::Pending
                })
                .await;
                continue;
            }
            let mut wire = mem::take(&mut *self.wire.borrow_mut());
            {
                let mut conn = self.conn.borrow_mut();
                while conn.wants_write() {
                    conn.write_tls(&mut wire)?;
                }
            }
            self.sending.set(true);
            let mut sending = Sending {
                stream: self,
                complete: false,
            };
            let (res, mut wire) = self.tcp.write_all(wire).await;
            res?;
            sending.complete = true;
            drop(sending);
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
            let mut conn = self.conn.borrow_mut();
            let mut plain = self.plain.borrow_mut();
            let mut input = &chunk[..];
            let mut result = Ok(());
            while !input.is_empty() {
                // rustls buffers a bounded amount of plaintext, so it has to
                // be drained as the records are fed in.
                match conn.read_tls(&mut input) {
                    Ok(0) => {
                        result = Err(invalid("rustls accepted no more TLS data"));
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
                            self.eof.set(true);
                            break;
                        }
                    }
                    Err(e) => {
                        result = Err(invalid(e));
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
    /// Cancel safe: dropping the future loses no data. (If it is dropped at
    /// the rare moment a TLS protocol message is being sent, sending fails
    /// from then on instead of continuing on a broken stream.)
    ///
    /// One task may receive while another one sends.
    pub async fn recv(&self) -> io::Result<Vec<u8>> {
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
        let mut sent = 0;
        while sent < buf.len() {
            // rustls accepts a bounded amount per call; send as it fills up.
            let accepted = self.conn.borrow_mut().writer().write(&buf[sent..]);
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

    /// Sends the TLS `close_notify` alert and closes the sending direction.
    pub async fn shutdown(&self) -> io::Result<()> {
        self.conn.borrow_mut().send_close_notify();
        self.flush_tls().await?;
        self.tcp.shutdown(Shutdown::Write)
    }

    /// The protocol agreed on through ALPN, if any (for example `http/1.1`).
    pub fn alpn_protocol(&self) -> Option<Vec<u8>> {
        self.conn.borrow().alpn_protocol().map(<[u8]>::to_vec)
    }

    /// The certificate chain the peer presented, its own certificate first:
    /// the server's on a client connection; on a server connection, the
    /// client's if client authentication is configured and the client sent
    /// one.
    pub fn peer_certificates(&self) -> Option<Vec<CertificateDer<'static>>> {
        self.conn.borrow().peer_certificates().map(<[_]>::to_vec)
    }

    /// `true` if the handshake resumed an earlier session instead of doing a
    /// full one.
    pub fn resumed(&self) -> bool {
        self.conn.borrow().handshake_kind() == Some(rustls::HandshakeKind::Resumed)
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
