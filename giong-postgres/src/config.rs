//! Where to connect and how.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use giong_tls::TlsConnector;

use crate::error::{Error, ErrorKind};

/// How the connection is encrypted.
#[derive(Clone)]
pub enum Tls {
    /// No TLS. Use it only on a network you trust: anyone on the path can
    /// read and change what is sent, though not a SCRAM password.
    Disable,
    /// TLS, checking the server's certificate with this connector: against
    /// its trusted authorities, and against the host name the client connects
    /// to. The connection fails if the server does not support TLS.
    Verify(TlsConnector),
}

impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tls::Disable => f.write_str("Disable"),
            Tls::Verify(_) => f.write_str("Verify(..)"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Host {
    /// A DNS name or an IP address.
    Tcp(String),
    /// The directory holding the server's Unix socket.
    Unix(PathBuf),
}

/// Connection settings: where the server is, who to log in as, and the limits
/// that apply to the connection.
///
/// Build one with [`Config::from_url`] or with the setters:
///
/// ```
/// use std::time::Duration;
/// use giong_postgres::Config;
///
/// let config = Config::from_url("postgres://app:secret@db.internal:5432/shop?sslmode=verify-full")?;
///
/// let same_place = Config::new()
///     .host("db.internal")
///     .user("app")
///     .password("secret")
///     .dbname("shop")
///     .statement_timeout(Duration::from_secs(5));
/// # Ok::<_, giong_postgres::Error>(())
/// ```
///
/// Every limit has a finite default, documented on its setter: a connection
/// never waits forever, and a result never grows without bound.
///
/// **TLS.** Unless [`tls`](Self::tls) or `sslmode` says otherwise, a TCP
/// connection to anything but a loopback address uses TLS and checks the
/// server's certificate against the certificate authorities installed on the
/// machine; connections over a Unix socket or to a loopback address do not
/// use TLS.
#[derive(Clone)]
pub struct Config {
    pub(crate) host: Host,
    pub(crate) port: u16,
    pub(crate) user: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) dbname: Option<String>,
    pub(crate) application_name: Option<String>,
    pub(crate) params: Vec<(String, String)>,
    tls: Option<Tls>,
    system_roots: Arc<OnceLock<Result<TlsConnector, String>>>,
    pub(crate) connect_timeout: Duration,
    pub(crate) statement_timeout: Duration,
    pub(crate) max_result_size: usize,
    pub(crate) statement_cache: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    /// Settings for `localhost:5432`, with no user yet and the default
    /// limits.
    pub fn new() -> Self {
        Self {
            host: Host::Tcp("localhost".to_owned()),
            port: 5432,
            user: None,
            password: None,
            dbname: None,
            application_name: None,
            params: Vec::new(),
            tls: None,
            system_roots: Arc::new(OnceLock::new()),
            connect_timeout: Duration::from_secs(10),
            statement_timeout: Duration::from_secs(30),
            max_result_size: 64 << 20,
            statement_cache: 256,
        }
    }

    /// The server's host: a DNS name, an IP address, or the absolute path of
    /// the directory holding its Unix socket (such as
    /// `/var/run/postgresql`).
    pub fn host(mut self, host: &str) -> Self {
        self.host = if host.starts_with('/') {
            Host::Unix(PathBuf::from(host))
        } else {
            let host = host
                .strip_prefix('[')
                .and_then(|h| h.strip_suffix(']'))
                .unwrap_or(host);
            Host::Tcp(host.to_owned())
        };
        self
    }

    /// The server's port, 5432 by default. For a Unix socket it picks the
    /// socket file, `.s.PGSQL.<port>`.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The user to log in as. Required.
    pub fn user(mut self, user: &str) -> Self {
        self.user = Some(user.to_owned());
        self
    }

    /// The password, if the server asks for one.
    pub fn password(mut self, password: &str) -> Self {
        self.password = Some(password.to_owned());
        self
    }

    /// The database; the server uses the user's name when it is not set.
    pub fn dbname(mut self, dbname: &str) -> Self {
        self.dbname = Some(dbname.to_owned());
        self
    }

    /// The name the server shows for this connection in `pg_stat_activity`
    /// and in its logs.
    pub fn application_name(mut self, name: &str) -> Self {
        self.application_name = Some(name.to_owned());
        self
    }

    /// Sets a server setting for the whole session when connecting, such as
    /// `search_path` or `TimeZone`.
    pub fn param(mut self, name: &str, value: &str) -> Self {
        self.params.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Whether and how to use TLS (see [the type's documentation](Self) for
    /// what happens when this is not set).
    pub fn tls(mut self, tls: Tls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// The longest that opening a connection may take, from name resolution
    /// to the end of authentication: 10 seconds by default.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// The longest a statement may run on the server before the server
    /// cancels it: 30 seconds by default.
    ///
    /// It is sent as the session's `statement_timeout` when connecting, so the
    /// server enforces it exactly, and only on the statement that runs too
    /// long. It is rounded up to whole milliseconds. Raise it for the
    /// connections that run migrations or reports, rather than turning it
    /// off with `SET`.
    pub fn statement_timeout(mut self, timeout: Duration) -> Self {
        self.statement_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// The largest result one statement may return, in bytes of row data:
    /// 64 MiB by default. A larger result fails with
    /// [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge), and the
    /// connection stays usable.
    pub fn max_result_size(mut self, bytes: usize) -> Self {
        self.max_result_size = bytes;
        self
    }

    /// How many prepared statements each connection keeps, 256 by default.
    ///
    /// Every statement is prepared on the server the first time a connection
    /// runs it, and kept, so running it again takes one round trip instead
    /// of two. The least recently used statement is closed when the cache is
    /// full. 0 prepares and closes every statement each time it runs (for
    /// connection poolers that cannot keep prepared statements).
    pub fn statement_cache_capacity(mut self, statements: usize) -> Self {
        self.statement_cache = statements;
        self
    }

    /// Reads a connection URL:
    ///
    /// ```text
    /// postgres://user:password@host:port/dbname?param=value&...
    /// ```
    ///
    /// `postgresql://` works too. User, password and database are
    /// percent-decoded. The host can be an IPv6 address in brackets, or, as
    /// with libpq, empty with the socket directory in a `host` parameter:
    /// `postgres:///shop?host=/var/run/postgresql`.
    ///
    /// The parameters understood are `sslmode`, `sslrootcert`, `host`,
    /// `port`, `user`, `password`, `dbname`, `application_name` and
    /// `connect_timeout` (seconds); any other is an error rather than being
    /// ignored. `sslmode` is `disable`, or `require`, `verify-ca` or
    /// `verify-full`, which all check the certificate and the host name
    /// (stricter than libpq, which checks nothing for `require`).
    /// `sslrootcert` is a PEM file of authorities to trust instead of the
    /// machine's, or `system`.
    pub fn from_url(url: &str) -> Result<Self, Error> {
        parse_url(url)
    }

    /// The URL this config was read from, or would be, without the password,
    /// for messages.
    pub(crate) fn target(&self) -> String {
        match &self.host {
            Host::Tcp(h) if h.contains(':') => format!("[{h}]:{}", self.port),
            Host::Tcp(h) => format!("{h}:{}", self.port),
            Host::Unix(dir) => self.socket_path(dir).display().to_string(),
        }
    }

    pub(crate) fn socket_path(&self, dir: &Path) -> PathBuf {
        dir.join(format!(".s.PGSQL.{}", self.port))
    }

    /// Whether the host is this machine: a Unix socket or a loopback address.
    pub(crate) fn is_local(&self) -> bool {
        match &self.host {
            Host::Unix(_) => true,
            Host::Tcp(h) => {
                h.eq_ignore_ascii_case("localhost")
                    || h.parse::<IpAddr>().is_ok_and(|ip| match ip {
                        IpAddr::V4(v4) => v4.is_loopback(),
                        IpAddr::V6(v6) => {
                            v6.is_loopback()
                                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
                        }
                    })
            }
        }
    }

    /// The connector to use, or `None` for no TLS.
    pub(crate) fn tls_connector(&self) -> Result<Option<TlsConnector>, Error> {
        match (&self.tls, &self.host) {
            (_, Host::Unix(_)) | (Some(Tls::Disable), _) => Ok(None),
            (Some(Tls::Verify(connector)), _) => Ok(Some(connector.clone())),
            (None, _) if self.is_local() => Ok(None),
            (None, Host::Tcp(host)) => {
                let roots = self
                    .system_roots
                    .get_or_init(|| TlsConnector::with_system_roots().map_err(|e| e.to_string()));
                match roots {
                    Ok(connector) => Ok(Some(connector.clone())),
                    Err(e) => Err(Error::new(
                        ErrorKind::Config,
                        format!(
                            "a connection to {host} uses TLS checked against the machine's \
                             certificate authorities, but they could not be loaded ({e}); give \
                             the server's authority with sslrootcert=/path/ca.pem or \
                             Config::tls, or add sslmode=disable if the network is trusted"
                        ),
                    )),
                }
            }
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field("dbname", &self.dbname)
            .field("application_name", &self.application_name)
            .field("params", &self.params)
            .field("tls", &self.tls)
            .field("connect_timeout", &self.connect_timeout)
            .field("statement_timeout", &self.statement_timeout)
            .field("max_result_size", &self.max_result_size)
            .field("statement_cache_capacity", &self.statement_cache)
            .finish()
    }
}

impl FromStr for Config {
    type Err = Error;

    fn from_str(url: &str) -> Result<Self, Error> {
        parse_url(url)
    }
}

fn percent_decode(s: &str, what: &str) -> Result<String, Error> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok())
                .ok_or_else(|| {
                    Error::config(format!(
                        "the {what} in the URL has a '%' that is not followed by two hex digits; \
                         write a literal % as %25"
                    ))
                })?;
            out.push(hex);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out)
        .map_err(|_| Error::config(format!("the {what} in the URL is not UTF-8 once decoded")))
}

fn parse_url(url: &str) -> Result<Config, Error> {
    let rest = url
        .strip_prefix("postgres://")
        .or_else(|| url.strip_prefix("postgresql://"))
        .ok_or_else(|| {
            Error::config(
                "a connection URL starts with postgres:// or postgresql://, \
                 as in postgres://user:password@host:5432/dbname",
            )
        })?;
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    let mut config = Config::new();
    let hostport = match authority.rsplit_once('@') {
        Some((userinfo, hostport)) => {
            let (user, password) = match userinfo.split_once(':') {
                Some((u, p)) => (u, Some(p)),
                None => (userinfo, None),
            };
            if !user.is_empty() {
                config.user = Some(percent_decode(user, "user")?);
            }
            if let Some(p) = password {
                config.password = Some(percent_decode(p, "password")?);
            }
            hostport
        }
        None => authority,
    };
    if hostport.contains(',') {
        return Err(Error::config(
            "the URL names several hosts; giong-postgres connects to one, so give one host",
        ));
    }
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (addr, after) = v6.split_once(']').ok_or_else(|| {
            Error::config("an IPv6 address in the URL is missing its closing ']'")
        })?;
        let port = match after {
            "" => None,
            p => Some(p.strip_prefix(':').ok_or_else(|| {
                Error::config("an IPv6 address in the URL must be followed by :port or nothing")
            })?),
        };
        (format!("[{addr}]"), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (percent_decode(h, "host")?, Some(p)),
            None => (percent_decode(hostport, "host")?, None),
        }
    };
    if !host.is_empty() {
        config = config.host(&host);
    }
    if let Some(port) = port {
        config.port = parse_port(port)?;
    }
    if let Some(db) = path.filter(|p| !p.is_empty()) {
        config.dbname = Some(percent_decode(db, "database name")?);
    }

    let mut sslmode = None;
    let mut sslrootcert = None;
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            Error::config(format!(
                "the URL parameter {pair:?} has no value; write {pair}=value"
            ))
        })?;
        let value = percent_decode(value, key)?;
        match key {
            "sslmode" => sslmode = Some(value),
            "sslrootcert" => sslrootcert = Some(value),
            "host" => config = config.host(&value),
            "port" => config.port = parse_port(&value)?,
            "user" => config.user = Some(value),
            "password" => config.password = Some(value),
            "dbname" => config.dbname = Some(value),
            "application_name" => config.application_name = Some(value),
            "connect_timeout" => {
                let secs: u64 = value.parse().ok().filter(|&s| s > 0).ok_or_else(|| {
                    Error::config(format!(
                        "connect_timeout={value} is not a whole number of seconds above zero"
                    ))
                })?;
                config.connect_timeout = Duration::from_secs(secs);
            }
            other => {
                return Err(Error::config(format!(
                    "the URL parameter {other:?} is not supported; the supported ones are \
                     sslmode, sslrootcert, host, port, user, password, dbname, \
                     application_name and connect_timeout"
                )))
            }
        }
    }

    let verify = || -> Result<Tls, Error> {
        match sslrootcert.as_deref() {
            None | Some("system") => Ok(Tls::Verify(TlsConnector::with_system_roots().map_err(
                |e| {
                    Error::io(
                        ErrorKind::Config,
                        "cannot load the machine's certificate authorities for sslmode",
                        e,
                    )
                },
            )?)),
            Some(path) => {
                let pem = std::fs::read(path).map_err(|e| {
                    Error::io(
                        ErrorKind::Config,
                        format!("cannot read sslrootcert {path:?}"),
                        e,
                    )
                })?;
                let connector = TlsConnector::with_root_pem(&pem).map_err(|e| {
                    Error::io(
                        ErrorKind::Config,
                        format!("sslrootcert {path:?} holds no usable certificate"),
                        e,
                    )
                })?;
                Ok(Tls::Verify(connector))
            }
        }
    };
    match sslmode.as_deref() {
        None if sslrootcert.is_some() => config.tls = Some(verify()?),
        None => {}
        Some("disable") => config.tls = Some(Tls::Disable),
        Some("require" | "verify-ca" | "verify-full") => config.tls = Some(verify()?),
        Some(mode @ ("prefer" | "allow")) => {
            return Err(Error::config(format!(
                "sslmode={mode} would silently connect without encryption when TLS fails; \
                 use sslmode=verify-full to require TLS, or sslmode=disable on a trusted network"
            )))
        }
        Some(other) => {
            return Err(Error::config(format!(
            "sslmode={other} is not a known mode; use disable, require, verify-ca or verify-full"
        )))
        }
    }
    Ok(config)
}

fn parse_port(port: &str) -> Result<u16, Error> {
    port.parse().ok().filter(|&p| p != 0).ok_or_else(|| {
        Error::config(format!(
            "the port {port:?} in the URL is not a number from 1 to 65535"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_url_is_read() {
        let c = Config::from_url(
            "postgres://app%40corp:p%3Ass%40w@db.internal:6543/shop%20db?application_name=api&connect_timeout=3&sslmode=disable",
        )
        .unwrap();
        assert_eq!(c.host, Host::Tcp("db.internal".into()));
        assert_eq!(c.port, 6543);
        assert_eq!(c.user.as_deref(), Some("app@corp"));
        assert_eq!(c.password.as_deref(), Some("p:ss@w"));
        assert_eq!(c.dbname.as_deref(), Some("shop db"));
        assert_eq!(c.application_name.as_deref(), Some("api"));
        assert_eq!(c.connect_timeout, Duration::from_secs(3));
        assert!(c.tls_connector().unwrap().is_none());
        assert!(!format!("{c:?}").contains("p:ss@w"));
    }

    #[test]
    fn hosts_can_be_ipv6_or_sockets() {
        let c = Config::from_url("postgresql://u@[::1]:5433").unwrap();
        assert_eq!(c.host, Host::Tcp("::1".into()));
        assert_eq!(c.port, 5433);
        assert_eq!(c.target(), "[::1]:5433");
        assert!(c.is_local());

        let c = Config::from_url("postgres:///shop?host=/var/run/postgresql&user=u").unwrap();
        assert_eq!(c.host, Host::Unix("/var/run/postgresql".into()));
        assert_eq!(c.target(), "/var/run/postgresql/.s.PGSQL.5432");
        let c = Config::from_url("postgres://u@%2Ftmp/shop").unwrap();
        assert_eq!(c.host, Host::Unix("/tmp".into()));
    }

    #[test]
    fn tls_is_on_for_remote_hosts_unless_disabled() {
        assert!(Config::new()
            .host("127.0.0.1")
            .tls_connector()
            .unwrap()
            .is_none());
        assert!(Config::new()
            .host("localhost")
            .tls_connector()
            .unwrap()
            .is_none());
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let connector = TlsConnector::with_root_pem(cert.cert.pem().as_bytes()).unwrap();
        let unix = Config::new()
            .host("/tmp")
            .tls(Tls::Verify(connector.clone()));
        assert!(unix.tls_connector().unwrap().is_none());
        let local = Config::new().host("127.0.0.1").tls(Tls::Verify(connector));
        assert!(local.tls_connector().unwrap().is_some());
        assert!(Config::new()
            .host("10.1.2.3")
            .tls(Tls::Disable)
            .tls_connector()
            .unwrap()
            .is_none());
        // The machine's authorities: present here, so TLS is on.
        if let Ok(Some(_)) = Config::new().host("db.example.com").tls_connector() {
        } else {
            panic!("expected TLS for a remote host");
        }
    }

    #[test]
    fn bad_urls_say_what_is_wrong() {
        let err = |url: &str| Config::from_url(url).unwrap_err().to_string();
        assert!(err("mysql://x").contains("postgres://"));
        assert!(err("postgres://u@h/db?sslmode=prefer").contains("silently"));
        assert!(err("postgres://u@h/db?sslmode=bogus").contains("not a known mode"));
        assert!(err("postgres://u@h/db?sslcert=x").contains("not supported"));
        assert!(err("postgres://u@h:99999/db").contains("1 to 65535"));
        assert!(err("postgres://u@h1,h2/db").contains("several hosts"));
        assert!(err("postgres://u@[::1/db").contains("closing"));
        assert!(err("postgres://u:%zz@h/db").contains("%25"));
        assert!(err("postgres://u@h/db?connect_timeout=0").contains("above zero"));
        assert!(err("postgres://u@h/db?sslrootcert=/nonexistent/ca.pem").contains("cannot read"));
        assert!(err("postgres://u@h/db?flag").contains("no value"));
    }

    #[test]
    fn statement_timeout_has_a_floor() {
        assert_eq!(
            Config::new()
                .statement_timeout(Duration::ZERO)
                .statement_timeout,
            Duration::from_millis(1)
        );
    }
}
