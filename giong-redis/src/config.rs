//! Where to connect and how.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use giong_tls::TlsConnector;

use crate::error::Error;

/// How the connection is encrypted.
#[derive(Clone)]
pub enum Tls {
    /// No TLS. Use it only on a network you trust: anyone on the path can
    /// read and change what is sent, the password included.
    Disable,
    /// TLS, checking the server's certificate with this connector: against
    /// its trusted authorities, and against the host name the client
    /// connects to.
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

/// Connection settings: where the server is, who to log in as, and the
/// limits that apply to the connection.
///
/// Build one with [`Config::from_url`] or with the setters:
///
/// ```
/// use std::time::Duration;
/// use giong_redis::Config;
///
/// let config = Config::from_url("rediss://app:secret@cache.internal:6380/2")?;
///
/// let same_place = Config::new()
///     .host("cache.internal")
///     .port(6380)
///     .user("app")
///     .password("secret")
///     .database(2)
///     .response_timeout(Duration::from_millis(500));
/// # Ok::<_, giong_redis::Error>(())
/// ```
///
/// Every limit has a finite default, documented on its setter: a command
/// never waits forever for its reply, and a reply never takes more memory
/// than [`max_reply_size`](Self::max_reply_size).
///
/// **TLS** is used when the URL starts with `rediss://`, or when
/// [`tls`](Self::tls) asks for it: the server's certificate is checked
/// against the certificate authorities installed on the machine (or those
/// given), and against the host name. `redis://` and a Unix socket do not
/// use TLS.
#[derive(Clone)]
pub struct Config {
    pub(crate) host: Host,
    pub(crate) port: u16,
    pub(crate) user: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) database: u32,
    pub(crate) client_name: Option<String>,
    tls: Option<Tls>,
    system_roots: Arc<OnceLock<Result<TlsConnector, String>>>,
    pub(crate) connect_timeout: Duration,
    pub(crate) response_timeout: Duration,
    pub(crate) max_reply_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

impl Config {
    /// The server and database, without user or password, for diagnostics:
    /// `cache.internal:6379/0`.
    pub(crate) fn describe(&self) -> String {
        let server = match &self.host {
            Host::Tcp(host) if host.contains(':') => format!("[{host}]:{}", self.port),
            Host::Tcp(host) => format!("{host}:{}", self.port),
            Host::Unix(path) => path.display().to_string(),
        };
        format!("{server}/{}", self.database)
    }

    /// Settings for `localhost:6379`, database 0, no password, and the
    /// default limits.
    pub fn new() -> Self {
        Self {
            host: Host::Tcp("localhost".to_owned()),
            port: 6379,
            user: None,
            password: None,
            database: 0,
            client_name: None,
            tls: None,
            system_roots: Arc::new(OnceLock::new()),
            connect_timeout: Duration::from_secs(10),
            response_timeout: Duration::from_secs(30),
            max_reply_size: 64 << 20,
        }
    }

    /// The server's host: a DNS name, an IP address, or the absolute path
    /// of its Unix socket (such as `/run/redis/redis.sock`).
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

    /// The server's port, 6379 by default.
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// The user to log in as, for servers with ACL users (Redis 6 and
    /// later). Without one, the password is the `default` user's.
    pub fn user(mut self, user: &str) -> Self {
        self.user = Some(user.to_owned());
        self
    }

    /// The password, if the server asks for one (`requirepass`, or the ACL
    /// user's).
    pub fn password(mut self, password: &str) -> Self {
        self.password = Some(password.to_owned());
        self
    }

    /// The database to use, 0 by default. Every connection selects it when
    /// it opens; `SELECT` cannot change it later.
    pub fn database(mut self, database: u32) -> Self {
        self.database = database;
        self
    }

    /// The name the server shows for the connection in `CLIENT LIST`.
    pub fn client_name(mut self, name: &str) -> Self {
        self.client_name = Some(name.to_owned());
        self
    }

    /// Whether and how to use TLS (see [the type's documentation](Self) for
    /// what happens when this is not set).
    pub fn tls(mut self, tls: Tls) -> Self {
        self.tls = Some(tls);
        self
    }

    /// The longest that opening a connection may take, from name resolution
    /// to the end of the handshake: 10 seconds by default.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// The longest a command waits for its reply: 30 seconds by default.
    /// Past it the command fails with
    /// [`ErrorKind::Timeout`](crate::ErrorKind::Timeout), though it may
    /// still run on the server, and the connection stays usable.
    ///
    /// Redis answers most commands in microseconds, so this is a safety
    /// net; set it near what the caller can wait (such as 500 ms for a
    /// cache). Blocking commands sent with
    /// [`Client::query_blocking`](crate::Client::query_blocking) are not
    /// bound by it: their own timeout argument bounds them.
    pub fn response_timeout(mut self, timeout: Duration) -> Self {
        self.response_timeout = timeout;
        self
    }

    /// The largest reply one command may return: 64 MiB by default, counted
    /// as the memory it takes once read, which is its bytes and 48 more for
    /// each element (each becomes a [`Value`](crate::Value)). A larger reply
    /// fails with [`ErrorKind::TooLarge`](crate::ErrorKind::TooLarge); it
    /// is read and thrown away as it arrives, and the connection stays
    /// usable.
    ///
    /// A reply of many small elements counts for much more than its bytes:
    /// `LRANGE` of a million 10-byte members is 17 MB as sent, and counts
    /// for 65 MB. Bytes alone, an array of 20 million nulls (57 MiB as
    /// sent) took 960 MB.
    pub fn max_reply_size(mut self, bytes: usize) -> Self {
        self.max_reply_size = bytes;
        self
    }

    /// Reads a connection URL:
    ///
    /// ```text
    /// redis://[[user]:password@]host[:port][/database][?param=value&...]
    /// rediss://...                       (the same, over TLS)
    /// redis+unix:///path/to/redis.sock[?db=N&...]
    /// unix:///path/to/redis.sock[?db=N&...]
    /// ```
    ///
    /// User and password are percent-decoded; `redis://:secret@host` gives
    /// a password alone. The host can be an IPv6 address in brackets.
    ///
    /// The parameters understood are `db` (the database, for a Unix socket),
    /// `client-name`, `connect-timeout` and `response-timeout` (seconds, as
    /// `0.5` or `10`), and `cacert` (a PEM file of certificate authorities
    /// to trust instead of the machine's, for `rediss://`); any other is an
    /// error rather than being ignored.
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

    /// The connector to use, or `None` for no TLS.
    pub(crate) fn tls_connector(&self) -> Result<Option<TlsConnector>, Error> {
        match (&self.tls, &self.host) {
            (_, Host::Unix(_)) | (None | Some(Tls::Disable), _) => Ok(None),
            (Some(Tls::Verify(connector)), _) => Ok(Some(connector.clone())),
        }
    }

    /// TLS checked against the machine's certificate authorities, loaded
    /// once for all clones of this configuration.
    fn system_tls(&self) -> Result<TlsConnector, Error> {
        let roots = self
            .system_roots
            .get_or_init(|| TlsConnector::with_system_roots().map_err(|e| e.to_string()));
        roots.clone().map_err(|e| {
            Error::config(format!(
                "rediss:// uses TLS checked against the machine's certificate authorities, but \
                 they could not be loaded ({e}); give the server's authority with \
                 ?cacert=/path/ca.pem or Config::tls"
            ))
        })
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
            .field("client_name", &self.client_name)
            .field("tls", &self.tls)
            .field("connect_timeout", &self.connect_timeout)
            .field("response_timeout", &self.response_timeout)
            .field("max_reply_size", &self.max_reply_size)
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

fn seconds(value: &str, name: &str) -> Result<Duration, Error> {
    value
        .parse::<f64>()
        .ok()
        .and_then(|s| Duration::try_from_secs_f64(s).ok())
        .ok_or_else(|| {
            Error::config(format!(
                "{name}={value:?} in the URL is not a number of seconds (such as 0.5 or 10)"
            ))
        })
}

fn parse_url(url: &str) -> Result<Config, Error> {
    parse(url).map_err(|e| {
        // An `@` after the first `/` or `?` is most likely the end of a
        // password holding an unescaped `/` or `?`: the URL was cut in
        // the wrong place, and the message would quote a piece of the
        // password as a port, a path or a parameter. Say what to fix
        // instead, quoting nothing.
        let Some((scheme, rest)) = url.split_once("://") else {
            return e;
        };
        let cut = rest.find(['/', '?']);
        if matches!(scheme, "redis" | "rediss") && cut.is_some_and(|i| rest[i..].contains('@')) {
            Error::config(
                "the URL could not be read, and it has an @ after a / or ?: if the password (or \
                 the user name) holds one of them, percent-encode it (/ as %2F, ? as %3F, @ as \
                 %40)",
            )
        } else {
            e
        }
    })
}

fn parse(url: &str) -> Result<Config, Error> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| {
        Error::config(
            "a connection URL starts with redis://, rediss:// or redis+unix://, as in \
             redis://:password@host:6379/0",
        )
    })?;
    let (rest, query) = match rest.split_once('?') {
        Some((r, q)) => (r, Some(q)),
        None => (rest, None),
    };
    let mut config = Config::new();
    let mut tls = false;
    match scheme {
        "redis" | "rediss" => {
            tls = scheme == "rediss";
            let (authority, path) = match rest.split_once('/') {
                Some((a, p)) => (a, p),
                None => (rest, ""),
            };
            let hostport = match authority.rsplit_once('@') {
                Some((userinfo, hostport)) => {
                    let (user, password) = match userinfo.split_once(':') {
                        Some((u, p)) => (u, Some(p)),
                        None => (userinfo, None),
                    };
                    if !user.is_empty() {
                        config.user = Some(percent_decode(user, "user name")?);
                    }
                    if let Some(p) = password {
                        config.password = Some(percent_decode(p, "password")?);
                    } else if !user.is_empty() {
                        return Err(Error::config(
                            "the URL has a user name but no password; Redis logs in with both \
                             (redis://user:password@host), or with a password alone \
                             (redis://:password@host)",
                        ));
                    }
                    hostport
                }
                None => authority,
            };
            let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
                let (host, after) = v6.split_once(']').ok_or_else(|| {
                    Error::config("an IPv6 address in the URL is missing its closing ]")
                })?;
                let port = match after {
                    "" => None,
                    p => Some(p.strip_prefix(':').ok_or_else(|| {
                        Error::config("after an IPv6 address in brackets comes :port or nothing")
                    })?),
                };
                (host, port)
            } else {
                match hostport.rsplit_once(':') {
                    Some((h, p)) => (h, Some(p)),
                    None => (hostport, None),
                }
            };
            if host.is_empty() {
                return Err(Error::config(
                    "the URL has no host; for a Unix socket write redis+unix:///path/to/redis.sock",
                ));
            }
            config.host = Host::Tcp(host.to_ascii_lowercase());
            if let Some(p) = port {
                config.port = p.parse().ok().filter(|&p| p != 0).ok_or_else(|| {
                    Error::config(format!(
                        "the port {p:?} in the URL is not a number from 1 to 65535"
                    ))
                })?;
            }
            let path = path.trim_end_matches('/');
            if !path.is_empty() {
                config.database = path.parse().map_err(|_| {
                    Error::config(format!(
                        "the path of the URL is the database number, not {path:?}"
                    ))
                })?;
            }
        }
        "redis+unix" | "unix" => {
            if !rest.starts_with('/') {
                return Err(Error::config(
                    "a Unix socket URL has an absolute path: redis+unix:///run/redis/redis.sock",
                ));
            }
            config.host = Host::Unix(PathBuf::from(percent_decode(rest, "socket path")?));
        }
        other => {
            return Err(Error::config(format!(
                "the URL scheme {other}:// is not one giong-redis knows; use redis://, rediss:// \
                 or redis+unix://"
            )))
        }
    }
    let mut cacert = None;
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value, name)?;
        match name {
            "db" => {
                config.database = value.parse().map_err(|_| {
                    Error::config(format!("db={value:?} in the URL is not a database number"))
                })?
            }
            "client-name" => config.client_name = Some(value),
            "connect-timeout" => config.connect_timeout = seconds(&value, name)?,
            "response-timeout" => config.response_timeout = seconds(&value, name)?,
            "cacert" => cacert = Some(value),
            other => {
                return Err(Error::config(format!(
                    "the URL parameter {other:?} is not one giong-redis knows (db, client-name, \
                     connect-timeout, response-timeout, cacert)"
                )))
            }
        }
    }
    if let Some(path) = cacert {
        if !tls {
            return Err(Error::config(
                "cacert is for TLS, which a redis:// URL does not use; write rediss://",
            ));
        }
        let pem = std::fs::read(&path)
            .map_err(|e| Error::config(format!("cannot read the cacert file {path}: {e}")))?;
        let connector = TlsConnector::with_root_pem(&pem)
            .map_err(|e| Error::config(format!("the cacert file {path} is not usable: {e}")))?;
        config.tls = Some(Tls::Verify(connector));
    } else if tls {
        config.tls = Some(Tls::Verify(config.system_tls()?));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_read_strictly() {
        let c =
            Config::from_url("redis://app:s%40cret@Cache.Internal:6380/3?client-name=web").unwrap();
        assert_eq!(c.host, Host::Tcp("cache.internal".into()));
        assert_eq!(c.port, 6380);
        assert_eq!(c.user.as_deref(), Some("app"));
        assert_eq!(c.password.as_deref(), Some("s@cret"));
        assert_eq!(c.database, 3);
        assert_eq!(c.client_name.as_deref(), Some("web"));
        assert!(c.tls_connector().unwrap().is_none());

        let c = Config::from_url("redis://:pw@[::1]/").unwrap();
        assert_eq!(c.host, Host::Tcp("::1".into()));
        assert_eq!(c.port, 6379);
        assert_eq!(c.user, None);
        assert_eq!(c.password.as_deref(), Some("pw"));
        assert_eq!(c.target(), "[::1]:6379");

        let c =
            Config::from_url("redis+unix:///run/redis.sock?db=5&response-timeout=0.25").unwrap();
        assert_eq!(c.host, Host::Unix("/run/redis.sock".into()));
        assert_eq!(c.database, 5);
        assert_eq!(c.response_timeout, Duration::from_millis(250));
        assert!(Config::from_url("unix:///tmp/r.sock").is_ok());

        for bad in [
            "http://host",
            "redis://",
            "redis://host:0",
            "redis://host:99999",
            "redis://host/x",
            "redis://host?nope=1",
            "redis://user@host",
            "redis://host?connect-timeout=soon",
            "redis://[::1",
            "redis+unix://relative.sock",
            "redis://host?cacert=/x.pem",
            "redis://:%zz@host",
        ] {
            let e = Config::from_url(bad).unwrap_err();
            assert_eq!(e.kind(), crate::ErrorKind::Config, "{bad}");
        }
    }

    #[test]
    fn a_password_cut_short_by_an_unescaped_character_is_not_quoted() {
        // Before, the password's pieces were quoted as a port, a path or a
        // parameter name: `the port "pa" in the URL is not a number`.
        for url in [
            "redis://:pa/ss@cache:6379",
            "redis://app:pa?ss@cache:6379",
            "redis://:secret/9@cache/0",
            "rediss://app:x?y@host",
        ] {
            let e = Config::from_url(url).unwrap_err().to_string();
            assert!(e.contains("percent-encode"), "{url}: {e}");
            for piece in ["pa", "ss", "secret", "9", "y"] {
                assert!(!e.contains(&format!("\"{piece}")), "{url}: {e}");
            }
        }
        // An @ in a parameter is fine when the URL reads.
        let c = Config::from_url("redis://host?client-name=me@here").unwrap();
        assert_eq!(c.client_name.as_deref(), Some("me@here"));
        // Encoded, the same passwords work.
        let c = Config::from_url("redis://app:pa%3Fss%2F@cache:6379").unwrap();
        assert_eq!(c.password.as_deref(), Some("pa?ss/"));
    }

    #[test]
    fn the_password_is_not_shown() {
        let c = Config::new().password("hunter2");
        assert!(!format!("{c:?}").contains("hunter2"));
    }
}
