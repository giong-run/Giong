//! Where to connect and how.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use giong_tls::TlsConnector;

use crate::error::{Error, ErrorKind};

/// How the connection is encrypted.
#[derive(Clone)]
pub enum Tls {
    /// No TLS. Use it only on a network you trust: anyone on the path can
    /// read and change what is sent, though not the password, which is
    /// always scrambled or encrypted.
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
    /// The path of the server's Unix socket.
    Unix(PathBuf),
}

/// Connection settings: where the server is, who to log in as, and the limits
/// that apply to the connection.
///
/// Build one with [`Config::from_url`] or with the setters:
///
/// ```
/// use std::time::Duration;
/// use giong_mysql::Config;
///
/// let config = Config::from_url("mysql://app:secret@db.internal:3306/shop?ssl-mode=VERIFY_IDENTITY")?;
///
/// let same_place = Config::new()
///     .host("db.internal")
///     .user("app")
///     .password("secret")
///     .database("shop")
///     .statement_timeout(Duration::from_secs(5));
/// # Ok::<_, giong_mysql::Error>(())
/// ```
///
/// Every limit has a finite default, documented on its setter: a connection
/// never waits forever, and a result never grows without bound.
///
/// **TLS.** Unless [`tls`](Self::tls) or `ssl-mode` says otherwise, a TCP
/// connection to anything but a loopback address uses TLS and checks the
/// server's certificate against the certificate authorities installed on the
/// machine; connections over a Unix socket or to a loopback address do not
/// use TLS.
///
/// **Passwords.** The password never crosses the network in clear text. With
/// `caching_sha2_password` (MySQL 8's default), a server that has not seen
/// the user since it started asks for the password itself: it is sent over
/// TLS or a Unix socket, or encrypted with the server's RSA key. That key is
/// asked of the server only on a loopback connection, where nobody can
/// replace it on the way; elsewhere, use TLS or give the key with
/// [`server_public_key`](Self::server_public_key).
#[derive(Clone)]
pub struct Config {
    pub(crate) host: Host,
    pub(crate) port: u16,
    pub(crate) user: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) application_name: Option<String>,
    pub(crate) session: Vec<(String, String)>,
    tls: Option<Tls>,
    system_roots: Arc<OnceLock<Result<TlsConnector, String>>>,
    pub(crate) server_public_key: Option<Arc<[u8]>>,
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
    /// Settings for `localhost:3306`, with no user yet and the default
    /// limits.
    pub fn new() -> Self {
        Self {
            host: Host::Tcp("localhost".to_owned()),
            port: 3306,
            user: None,
            password: None,
            database: None,
            application_name: None,
            session: Vec::new(),
            tls: None,
            system_roots: Arc::new(OnceLock::new()),
            server_public_key: None,
            connect_timeout: Duration::from_secs(10),
            statement_timeout: Duration::from_secs(30),
            max_result_size: 64 << 20,
            statement_cache: 256,
        }
    }

    /// The server's host: a DNS name, an IP address, or the absolute path of
    /// its Unix socket (such as `/var/run/mysqld/mysqld.sock`).
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

    /// The server's port, 3306 by default.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The user to log in as. Required.
    pub fn user(mut self, user: &str) -> Self {
        self.user = Some(user.to_owned());
        self
    }

    /// The password, if the user has one.
    pub fn password(mut self, password: &str) -> Self {
        self.password = Some(password.to_owned());
        self
    }

    /// The database to use; none by default, so that SQL must name its
    /// tables as `database.table`.
    pub fn database(mut self, database: &str) -> Self {
        self.database = Some(database.to_owned());
        self
    }

    /// The name the server shows for this connection, as the `program_name`
    /// connection attribute (in `performance_schema.session_connect_attrs`).
    pub fn application_name(mut self, name: &str) -> Self {
        self.application_name = Some(name.to_owned());
        self
    }

    /// Sets a session variable when connecting: `SET SESSION name = value`.
    ///
    /// `value` is SQL, so a string needs quotes:
    /// `.session_variable("sql_mode", "'STRICT_ALL_TABLES'")` or
    /// `.session_variable("transaction_isolation", "'READ-COMMITTED'")`. The
    /// name must be a plain identifier. Do not set `time_zone` (see
    /// [`types`](crate::types)) or `max_execution_time` (see
    /// [`statement_timeout`](Self::statement_timeout)).
    pub fn session_variable(mut self, name: &str, value: &str) -> Self {
        self.session.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Whether and how to use TLS (see [the type's documentation](Self) for
    /// what happens when this is not set).
    pub fn tls(mut self, tls: Tls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// The server's RSA public key (PEM), to encrypt the password with
    /// when the server asks for it on a connection without TLS. Copy it from
    /// the server's data directory (`public_key.pem`) or from
    /// `SHOW STATUS LIKE 'Caching_sha2_password_rsa_public_key'`.
    pub fn server_public_key(mut self, pem: &[u8]) -> Self {
        self.server_public_key = Some(pem.into());
        self
    }

    /// The longest that opening a connection may take, from name resolution
    /// to the end of authentication: 10 seconds by default.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// The longest a `SELECT` may run on the server before the server stops
    /// it: 30 seconds by default.
    ///
    /// It is set as the session's `max_execution_time` when connecting, which
    /// MySQL applies to read-only `SELECT` statements only. Statements that
    /// write are bounded by how long they may wait for a lock
    /// (`innodb_lock_wait_timeout`, 50 seconds unless the server sets
    /// otherwise), not by this. It is rounded up to whole milliseconds.
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
    /// full. 0 prepares and closes every statement each time it runs. The
    /// server's `max_prepared_stmt_count` (16,382 by default) bounds the
    /// total over all connections.
    pub fn statement_cache_capacity(mut self, statements: usize) -> Self {
        self.statement_cache = statements;
        self
    }

    /// Reads a connection URL:
    ///
    /// ```text
    /// mysql://user:password@host:port/database?param=value&...
    /// ```
    ///
    /// User, password and database are percent-decoded. The host can be an
    /// IPv6 address in brackets, or empty with the socket in a `socket`
    /// parameter: `mysql://app@/shop?socket=/var/run/mysqld/mysqld.sock`.
    ///
    /// The parameters understood are `ssl-mode`, `ssl-ca`, `socket`,
    /// `server-public-key-path`, `connect-timeout` (seconds) and
    /// `application-name`; any other is an error rather than being ignored.
    /// `ssl-mode` is `DISABLED`, or `REQUIRED`, `VERIFY_CA` or
    /// `VERIFY_IDENTITY`, which all check the certificate and the host name
    /// (stricter than MySQL's own client, which checks nothing for
    /// `REQUIRED`). `ssl-ca` is a PEM file of authorities to trust instead of
    /// the machine's.
    pub fn from_url(url: &str) -> Result<Self, Error> {
        parse_url(url)
    }

    /// Where the server is, for messages.
    pub(crate) fn target(&self) -> String {
        match &self.host {
            Host::Tcp(h) if h.contains(':') => format!("[{h}]:{}", self.port),
            Host::Tcp(h) => format!("{h}:{}", self.port),
            Host::Unix(path) => path.display().to_string(),
        }
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
                             the server's authority with ssl-ca=/path/ca.pem or Config::tls, or \
                             add ssl-mode=DISABLED if the network is trusted"
                        ),
                    )),
                }
            }
        }
    }

    /// The `SET` statement run after logging in.
    pub(crate) fn session_sql(&self) -> Result<String, Error> {
        let ms = self.statement_timeout.as_micros().div_ceil(1000);
        let mut sql = format!("SET time_zone = '+00:00', max_execution_time = {ms}");
        for (name, value) in &self.session {
            let plain =
                !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !plain {
                return Err(Error::config(format!(
                    "the session variable name {name:?} is not a plain identifier (letters, \
                     digits and _)"
                )));
            }
            if name.eq_ignore_ascii_case("time_zone")
                || name.eq_ignore_ascii_case("max_execution_time")
            {
                return Err(Error::config(format!(
                    "the session variable {name} is set by giong-mysql itself; use \
                     Config::statement_timeout for max_execution_time, and keep the time zone \
                     UTC"
                )));
            }
            sql.push_str(&format!(", SESSION {name} = {value}"));
        }
        Ok(sql)
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<hidden>"))
            .field("database", &self.database)
            .field("application_name", &self.application_name)
            .field("session", &self.session)
            .field("tls", &self.tls)
            .field("server_public_key", &self.server_public_key.is_some())
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
    let rest = url.strip_prefix("mysql://").ok_or_else(|| {
        Error::config(
            "a connection URL starts with mysql://, as in mysql://user:password@host:3306/database",
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
            "the URL names several hosts; giong-mysql connects to one, so give one host",
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
        config.database = Some(percent_decode(db, "database name")?);
    }

    let mut ssl_mode = None;
    let mut ssl_ca = None;
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            Error::config(format!(
                "the URL parameter {pair:?} has no value; write {pair}=value"
            ))
        })?;
        let value = percent_decode(value, key)?;
        match key {
            "ssl-mode" => ssl_mode = Some(value.to_ascii_uppercase()),
            "ssl-ca" => ssl_ca = Some(value),
            "socket" => {
                if !value.starts_with('/') {
                    return Err(Error::config(format!(
                        "socket={value} is not an absolute path to the server's socket file"
                    )));
                }
                config = config.host(&value);
            }
            "server-public-key-path" => {
                let pem = std::fs::read(&value).map_err(|e| {
                    Error::io(
                        ErrorKind::Config,
                        format!("cannot read server-public-key-path {value:?}"),
                        e,
                    )
                })?;
                config.server_public_key = Some(pem.into());
            }
            "application-name" => config.application_name = Some(value),
            "connect-timeout" => {
                let secs: u64 = value.parse().ok().filter(|&s| s > 0).ok_or_else(|| {
                    Error::config(format!(
                        "connect-timeout={value} is not a whole number of seconds above zero"
                    ))
                })?;
                config.connect_timeout = Duration::from_secs(secs);
            }
            other => {
                return Err(Error::config(format!(
                    "the URL parameter {other:?} is not supported; the supported ones are \
                     ssl-mode, ssl-ca, socket, server-public-key-path, connect-timeout and \
                     application-name"
                )))
            }
        }
    }

    let verify = || -> Result<Tls, Error> {
        match ssl_ca.as_deref() {
            None => Ok(Tls::Verify(TlsConnector::with_system_roots().map_err(
                |e| {
                    Error::io(
                        ErrorKind::Config,
                        "cannot load the machine's certificate authorities for ssl-mode",
                        e,
                    )
                },
            )?)),
            Some(path) => {
                let pem = std::fs::read(path).map_err(|e| {
                    Error::io(ErrorKind::Config, format!("cannot read ssl-ca {path:?}"), e)
                })?;
                let connector = TlsConnector::with_root_pem(&pem).map_err(|e| {
                    Error::io(
                        ErrorKind::Config,
                        format!("ssl-ca {path:?} holds no usable certificate"),
                        e,
                    )
                })?;
                Ok(Tls::Verify(connector))
            }
        }
    };
    match ssl_mode.as_deref() {
        None if ssl_ca.is_some() => config.tls = Some(verify()?),
        None => {}
        Some("DISABLED") => config.tls = Some(Tls::Disable),
        Some("REQUIRED" | "VERIFY_CA" | "VERIFY_IDENTITY") => config.tls = Some(verify()?),
        Some("PREFERRED") => {
            return Err(Error::config(
                "ssl-mode=PREFERRED would silently connect without encryption when TLS fails; \
                 use ssl-mode=VERIFY_IDENTITY to require TLS, or ssl-mode=DISABLED on a trusted \
                 network",
            ))
        }
        Some(other) => {
            return Err(Error::config(format!(
                "ssl-mode={other} is not a known mode; use DISABLED, REQUIRED, VERIFY_CA or \
                 VERIFY_IDENTITY"
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
            "mysql://app%40corp:p%3Ass%40w@db.internal:3307/shop%20db?application-name=api&connect-timeout=3&ssl-mode=disabled",
        )
        .unwrap();
        assert_eq!(c.host, Host::Tcp("db.internal".into()));
        assert_eq!(c.port, 3307);
        assert_eq!(c.user.as_deref(), Some("app@corp"));
        assert_eq!(c.password.as_deref(), Some("p:ss@w"));
        assert_eq!(c.database.as_deref(), Some("shop db"));
        assert_eq!(c.application_name.as_deref(), Some("api"));
        assert_eq!(c.connect_timeout, Duration::from_secs(3));
        assert!(c.tls_connector().unwrap().is_none());
        assert!(!format!("{c:?}").contains("p:ss@w"));
    }

    #[test]
    fn hosts_can_be_ipv6_or_sockets() {
        let c = Config::from_url("mysql://u@[::1]:3307").unwrap();
        assert_eq!(c.host, Host::Tcp("::1".into()));
        assert_eq!(c.target(), "[::1]:3307");
        assert!(c.is_local());

        let c = Config::from_url("mysql://u@/shop?socket=/var/run/mysqld/mysqld.sock").unwrap();
        assert_eq!(c.host, Host::Unix("/var/run/mysqld/mysqld.sock".into()));
        assert_eq!(c.target(), "/var/run/mysqld/mysqld.sock");
        let c = Config::from_url("mysql://u@%2Ftmp%2Fmysql.sock/shop").unwrap();
        assert_eq!(c.host, Host::Unix("/tmp/mysql.sock".into()));
    }

    #[test]
    fn tls_is_on_for_remote_hosts_unless_disabled() {
        assert!(Config::new()
            .host("127.0.0.1")
            .tls_connector()
            .unwrap()
            .is_none());
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let connector = TlsConnector::with_root_pem(cert.cert.pem().as_bytes()).unwrap();
        let unix = Config::new()
            .host("/tmp/mysql.sock")
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
        if let Ok(Some(_)) = Config::new().host("db.example.com").tls_connector() {
        } else {
            panic!("expected TLS for a remote host");
        }
    }

    #[test]
    fn bad_urls_say_what_is_wrong() {
        let err = |url: &str| Config::from_url(url).unwrap_err().to_string();
        assert!(err("postgres://x").contains("mysql://"));
        assert!(err("mysql://u@h/db?ssl-mode=PREFERRED").contains("silently"));
        assert!(err("mysql://u@h/db?ssl-mode=bogus").contains("not a known mode"));
        assert!(err("mysql://u@h/db?sslmode=disable").contains("not supported"));
        assert!(err("mysql://u@h:99999/db").contains("1 to 65535"));
        assert!(err("mysql://u@h1,h2/db").contains("several hosts"));
        assert!(err("mysql://u@[::1/db").contains("closing"));
        assert!(err("mysql://u:%zz@h/db").contains("%25"));
        assert!(err("mysql://u@h/db?connect-timeout=0").contains("above zero"));
        assert!(err("mysql://u@h/db?ssl-ca=/nonexistent/ca.pem").contains("cannot read"));
        assert!(err("mysql://u@h/db?socket=relative.sock").contains("absolute"));
        assert!(err("mysql://u@h/db?flag").contains("no value"));
    }

    #[test]
    fn session_variables_are_checked() {
        let c = Config::new()
            .statement_timeout(Duration::from_micros(2500))
            .session_variable("sql_mode", "'STRICT_ALL_TABLES'");
        assert_eq!(
            c.session_sql().unwrap(),
            "SET time_zone = '+00:00', max_execution_time = 3, SESSION sql_mode = \
             'STRICT_ALL_TABLES'"
        );
        let bad = Config::new().session_variable("a; DROP TABLE x", "1");
        assert!(bad
            .session_sql()
            .unwrap_err()
            .to_string()
            .contains("identifier"));
        let tz = Config::new().session_variable("time_zone", "'+07:00'");
        assert!(tz.session_sql().unwrap_err().to_string().contains("UTC"));
        assert_eq!(
            Config::new()
                .statement_timeout(Duration::ZERO)
                .statement_timeout,
            Duration::from_millis(1)
        );
    }
}
