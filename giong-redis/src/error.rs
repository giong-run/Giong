//! Errors: what went wrong, and what to do about it.

use std::fmt;
use std::io;
use std::sync::Arc;

use giong::error::Code;

/// An error from giong-redis.
///
/// Every error says what happened and, where there is one, what to do about
/// it. It is shown with the giong error code of its kind in front, and the
/// link to that code's page at the end: `GIONG-E3204: WRONGTYPE Operation
/// against a key holding the wrong kind of value. See https://…/E3204.md`.
/// [`kind`](Self::kind) tells errors apart in code; an error the server
/// sent is in [`server`](Self::server), with its code (`WRONGTYPE`,
/// `NOSCRIPT`...) in [`server_code`](Self::server_code).
///
/// Errors are cheap to clone, and convert into [`std::io::Error`], so `?`
/// works in functions that return `io::Result`; there,
/// [`giong::error::code`] finds the code, and
/// [`giong::error::Error::get_ref`] gives this error back.
#[derive(Clone)]
pub struct Error(Arc<Inner>);

struct Inner {
    kind: ErrorKind,
    message: String,
    server: Option<ServerError>,
    source: Option<io::Error>,
}

/// What kind of error it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The configuration or the connection URL is not valid.
    Config,
    /// Opening the connection failed: name resolution, TCP, TLS, the
    /// handshake, or the connect timeout.
    Connect,
    /// The server refused the user name or the password.
    Auth,
    /// The server answered the command with an error. [`Error::server`] has
    /// it. Also a transaction the server did not run because a watched key
    /// changed, outside a [`Watch`](crate::Watch).
    Server,
    /// A reply could not be converted to the Rust type asked for, such as a
    /// missing key (nil) read as a `String` instead of an `Option<String>`.
    Type,
    /// The command cannot be sent this way, and nothing was sent: a command
    /// that would change the connection for the other tasks sharing it
    /// (`MULTI`, `SELECT`, `SUBSCRIBE`...), or a blocking one (`BLPOP`...)
    /// sent with [`Client::query`](crate::Client::query). The message says
    /// what to use instead.
    Command,
    /// The reply was larger than
    /// [`Config::max_reply_size`](crate::Config::max_reply_size). The
    /// connection stays usable.
    TooLarge,
    /// The connection is closed: the server closed it, the network failed,
    /// or an earlier error broke it. Open a new one (a
    /// [`Pool`](crate::Pool) does this by itself).
    Closed,
    /// No pooled connection became free within
    /// [`Pool::wait_timeout`](crate::Pool::wait_timeout).
    PoolTimeout,
    /// The server sent something that is not RESP. The connection is
    /// closed.
    Protocol,
    /// The reply did not arrive within
    /// [`Config::response_timeout`](crate::Config::response_timeout). The
    /// command may still have run.
    Timeout,
}

impl ErrorKind {
    /// The giong error code for errors of this kind, `GIONG-E3201` to
    /// `GIONG-E3211`.
    pub fn giong_code(self) -> Code {
        match self {
            ErrorKind::Config => Code::RedisConfig,
            ErrorKind::Connect => Code::RedisConnect,
            ErrorKind::Auth => Code::RedisAuth,
            ErrorKind::Server => Code::RedisServer,
            ErrorKind::Type => Code::RedisType,
            ErrorKind::Command => Code::RedisCommand,
            ErrorKind::TooLarge => Code::RedisTooLarge,
            ErrorKind::Closed => Code::RedisClosed,
            ErrorKind::PoolTimeout => Code::RedisPoolTimeout,
            ErrorKind::Protocol => Code::RedisProtocol,
            ErrorKind::Timeout => Code::RedisTimeout,
        }
    }
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            server: None,
            source: None,
        }))
    }

    pub(crate) fn io(kind: ErrorKind, message: impl Into<String>, source: io::Error) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            server: None,
            source: Some(source),
        }))
    }

    pub(crate) fn with_server(kind: ErrorKind, message: impl Into<String>, e: ServerError) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            server: Some(e),
            source: None,
        }))
    }

    /// An error for a reply that cannot be converted, for implementations
    /// of [`FromValue`](crate::FromValue). Say what the reply was and what
    /// would work instead.
    pub fn conversion(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Type, message)
    }

    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    pub(crate) fn command(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Command, message)
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::Protocol,
            format!(
                "{}; the connection was closed (is this really a Redis server?)",
                message.into()
            ),
        )
    }

    pub(crate) fn closed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Closed, message)
    }

    /// What kind of error this is.
    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// The giong error code, the one shown in front of the message: one for
    /// each [`ErrorKind`]. For the server's own code, see
    /// [`server_code`](Self::server_code).
    pub fn giong_code(&self) -> Code {
        self.0.kind.giong_code()
    }

    /// The error the server sent, for [`ErrorKind::Server`] (and for
    /// [`ErrorKind::Auth`] when the server refused the credentials). `None`
    /// for a transaction that did not run because a watched key changed,
    /// which the server answers with nil, not an error.
    pub fn server(&self) -> Option<&ServerError> {
        self.0.server.as_ref()
    }

    /// The code of the server's error: its first word, such as `ERR`,
    /// `WRONGTYPE`, `NOSCRIPT` or `BUSY`.
    pub fn server_code(&self) -> Option<&str> {
        self.0.server.as_ref().map(|e| e.code())
    }

    /// Whether trying again later may succeed: the connection failed or
    /// closed, a timeout passed, or the server is busy or not ready
    /// (`LOADING`, `BUSY`, `TRYAGAIN`, `MASTERDOWN`, `CLUSTERDOWN`).
    ///
    /// A command that is not idempotent (`INCR`, `LPUSH`) may have run
    /// before a [`Closed`](ErrorKind::Closed) or
    /// [`Timeout`](ErrorKind::Timeout) error: retrying it may run it twice.
    pub fn is_transient(&self) -> bool {
        match self.0.kind {
            ErrorKind::Connect
            | ErrorKind::Closed
            | ErrorKind::Timeout
            | ErrorKind::PoolTimeout => true,
            ErrorKind::Server => matches!(
                self.server_code(),
                Some("LOADING" | "BUSY" | "TRYAGAIN" | "MASTERDOWN" | "CLUSTERDOWN")
            ),
            _ => false,
        }
    }

    /// What happened: the message, the server's error and the cause, without
    /// the code and the link.
    fn what(&self) -> String {
        let inner = &self.0;
        let mut parts = Vec::with_capacity(3);
        if let Some(e) = &inner.server {
            parts.push(e.to_string());
        }
        if !inner.message.is_empty() {
            parts.push(inner.message.clone());
        }
        if let Some(source) = &inner.source {
            parts.push(source.to_string());
        }
        parts.join(": ")
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.giong_code();
        let what = self.what();
        let stop = if what.ends_with('.') { "" } else { "." };
        write!(f, "{code}: {what}{stop} See {}", code.url())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("kind", &self.0.kind)
            .field("message", &self.to_string())
            .finish()
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.0.source {
            Some(e) => Some(e),
            None => self.0.server.as_ref().map(|e| e as _),
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match (&e.0.source, e.kind()) {
            (Some(source), _) => source.kind(),
            (None, ErrorKind::Config | ErrorKind::Command) => io::ErrorKind::InvalidInput,
            (None, ErrorKind::Auth) => io::ErrorKind::PermissionDenied,
            (None, ErrorKind::Type | ErrorKind::Protocol) => io::ErrorKind::InvalidData,
            (None, ErrorKind::PoolTimeout | ErrorKind::Timeout) => io::ErrorKind::TimedOut,
            (None, ErrorKind::Closed) => io::ErrorKind::ConnectionAborted,
            (None, _) => io::ErrorKind::Other,
        };
        io::Error::new(kind, giong::error::Error::wrap(e.giong_code(), e))
    }
}

/// An error reply from the server, such as `WRONGTYPE Operation against a
/// key holding the wrong kind of value`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerError {
    /// The whole line, code included.
    line: String,
    /// Where the code ends in `line`.
    code_end: usize,
}

impl ServerError {
    pub(crate) fn new(line: String) -> Self {
        let code_end = line
            .find(' ')
            .filter(|&i| {
                line[..i]
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b == b'_')
            })
            .unwrap_or(0);
        Self { line, code_end }
    }

    /// The error's code, its first word in capitals: `ERR`, `WRONGTYPE`,
    /// `NOSCRIPT`, `BUSY`... Empty when the line does not start with one.
    pub fn code(&self) -> &str {
        &self.line[..self.code_end]
    }

    /// The message after the code.
    pub fn message(&self) -> &str {
        self.line[self.code_end..].trim_start()
    }
}

impl fmt::Display for ServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.line)
    }
}

impl std::error::Error for ServerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_with_its_giong_code_and_link() {
        let e = Error::with_server(
            ErrorKind::Server,
            "",
            ServerError::new(
                "WRONGTYPE Operation against a key holding the wrong kind of value".into(),
            ),
        );
        assert_eq!(
            e.to_string(),
            "GIONG-E3204: WRONGTYPE Operation against a key holding the wrong kind of value. \
             See https://github.com/giong-run/Giong/blob/main/docs/errors/E3204.md"
        );
        assert_eq!(e.server_code(), Some("WRONGTYPE"));
        assert_eq!(
            e.server().unwrap().message(),
            "Operation against a key holding the wrong kind of value"
        );
        assert!(!e.is_transient());
    }

    #[test]
    fn a_line_without_a_code_has_an_empty_one() {
        let e = ServerError::new("something odd".into());
        assert_eq!(e.code(), "");
        assert_eq!(e.message(), "something odd");
        assert_eq!(ServerError::new("BUSY".into()).code(), "");
        assert_eq!(
            ServerError::new("LOADING Redis is loading".into()).code(),
            "LOADING"
        );
    }

    #[test]
    fn the_code_and_the_error_survive_the_conversion_to_io() {
        let e = Error::new(ErrorKind::PoolTimeout, "no connection became free");
        let shown = e.to_string();
        let io: io::Error = e.into();
        assert_eq!(io.kind(), io::ErrorKind::TimedOut);
        assert_eq!(io.to_string(), shown);
        assert_eq!(giong::error::code(&io), Some(Code::RedisPoolTimeout));
        let wrapped = io.get_ref().unwrap();
        let wrapped = wrapped.downcast_ref::<giong::error::Error>().unwrap();
        let back = wrapped.get_ref().unwrap().downcast_ref::<Error>().unwrap();
        assert_eq!(back.kind(), ErrorKind::PoolTimeout);
    }

    #[test]
    fn every_kind_has_its_own_code() {
        let kinds = [
            ErrorKind::Config,
            ErrorKind::Connect,
            ErrorKind::Auth,
            ErrorKind::Server,
            ErrorKind::Type,
            ErrorKind::Command,
            ErrorKind::TooLarge,
            ErrorKind::Closed,
            ErrorKind::PoolTimeout,
            ErrorKind::Protocol,
            ErrorKind::Timeout,
        ];
        let codes: std::collections::HashSet<_> = kinds.iter().map(|k| k.giong_code()).collect();
        assert_eq!(codes.len(), kinds.len());
        assert!(codes.iter().all(|c| (3201..3300).contains(&c.number())));
    }
}
