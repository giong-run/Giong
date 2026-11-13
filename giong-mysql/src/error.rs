//! Errors: what went wrong, and what to do about it.

use std::fmt;
use std::io;
use std::sync::Arc;

use giong::error::Code;

/// An error from giong-mysql.
///
/// Every error says what happened and, where there is one, what to do about
/// it. It is shown with the giong error code of its kind in front, and the
/// link to that code's page at the end: `GIONG-E3103: access denied for
/// user 'app'@'10.0.0.7'. See https://…/E3103.md`.
/// [`kind`](Self::kind) tells errors apart in code; errors the server
/// reported carry them in [`db`](Self::db), with MySQL's error number in
/// [`code`](Self::code).
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
    db: Option<DbError>,
    source: Option<io::Error>,
}

/// What kind of error it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The configuration or the connection URL is not valid.
    Config,
    /// Opening the connection failed: name resolution, TCP, TLS, or the
    /// connect timeout.
    Connect,
    /// The server refused the credentials, or asked for an authentication
    /// method this crate does not support.
    Auth,
    /// The server reported an error for the statement. [`Error::db`] has the
    /// details.
    Db,
    /// A value could not be converted between Rust and MySQL: a parameter,
    /// or a column read with [`Row::get`](crate::Row::get).
    Type,
    /// [`query_one`](crate::Client::query_one) found no row or more than one,
    /// or [`insert`](crate::Client::insert) found no generated id.
    RowCount,
    /// The result was larger than
    /// [`Config::max_result_size`](crate::Config::max_result_size).
    TooLarge,
    /// The connection is closed: the server closed it, the network failed, or
    /// an earlier error broke it. Open a new one (a [`Pool`](crate::Pool) does
    /// this by itself).
    Closed,
    /// No pooled connection became free within
    /// [`Pool::wait_timeout`](crate::Pool::wait_timeout).
    PoolTimeout,
    /// The server sent something this crate cannot understand. The connection
    /// is closed.
    Protocol,
    /// The statement cannot be sent this way, and was not: a transaction
    /// statement on the connections a [`Pool`](crate::Pool) shares between
    /// tasks. The message says what to use instead.
    Statement,
}

impl ErrorKind {
    /// The giong error code for errors of this kind, `GIONG-E3101` to
    /// `GIONG-E3111`.
    pub fn giong_code(self) -> Code {
        match self {
            ErrorKind::Config => Code::MysqlConfig,
            ErrorKind::Connect => Code::MysqlConnect,
            ErrorKind::Auth => Code::MysqlAuth,
            ErrorKind::Db => Code::MysqlServer,
            ErrorKind::Type => Code::MysqlType,
            ErrorKind::RowCount => Code::MysqlRowCount,
            ErrorKind::TooLarge => Code::MysqlTooLarge,
            ErrorKind::Closed => Code::MysqlClosed,
            ErrorKind::PoolTimeout => Code::MysqlPoolTimeout,
            ErrorKind::Protocol => Code::MysqlProtocol,
            ErrorKind::Statement => Code::MysqlStatement,
        }
    }
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            db: None,
            source: None,
        }))
    }

    pub(crate) fn io(kind: ErrorKind, message: impl Into<String>, source: io::Error) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            db: None,
            source: Some(source),
        }))
    }

    pub(crate) fn from_db(db: DbError, note: Option<String>) -> Self {
        Self(Arc::new(Inner {
            kind: ErrorKind::Db,
            message: note.unwrap_or_default(),
            db: Some(db),
            source: None,
        }))
    }

    pub(crate) fn with_db(kind: ErrorKind, message: impl Into<String>, db: DbError) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            db: Some(db),
            source: None,
        }))
    }

    /// An error for a value that cannot be converted, for implementations of
    /// [`ToSql`](crate::ToSql) and [`FromSql`](crate::FromSql). Say what the
    /// value was and what would work instead.
    pub fn conversion(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Type, message)
    }

    pub(crate) fn config(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Config, message)
    }

    pub(crate) fn protocol(message: impl Into<String>) -> Self {
        Self::new(
            ErrorKind::Protocol,
            format!(
                "{}; the connection was closed (is this really a MySQL server?)",
                message.into()
            ),
        )
    }

    pub(crate) fn closed(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Closed, message)
    }

    /// The same error with `context` in front of its message.
    pub(crate) fn context(&self, context: &str) -> Self {
        Self(Arc::new(Inner {
            kind: self.0.kind,
            message: if self.0.message.is_empty() {
                context.to_owned()
            } else {
                format!("{context}: {}", self.0.message)
            },
            db: self.0.db.clone(),
            source: self
                .0
                .source
                .as_ref()
                .map(|e| io::Error::new(e.kind(), e.to_string())),
        }))
    }

    /// What kind of error this is.
    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// The giong error code, the one shown in front of the message: one
    /// for each [`ErrorKind`]. For the server's own code, see
    /// [`code`](Self::code).
    pub fn giong_code(&self) -> Code {
        self.0.kind.giong_code()
    }

    /// The error the server reported, for [`ErrorKind::Db`] (and for
    /// [`ErrorKind::Auth`] when the server refused the credentials).
    pub fn db(&self) -> Option<&DbError> {
        self.0.db.as_ref()
    }

    /// MySQL's number for the server's error, such as 1062 for a duplicate
    /// key. The numbers are listed in the MySQL manual's "Server Error
    /// Message Reference".
    pub fn code(&self) -> Option<u16> {
        self.0.db.as_ref().map(|db| db.code())
    }
}

impl Error {
    /// What happened: the message, the server's error and the cause, without
    /// the code and the link.
    fn what(&self) -> String {
        let inner = &self.0;
        let mut parts = Vec::with_capacity(3);
        if !inner.message.is_empty() {
            parts.push(inner.message.clone());
        }
        if let Some(db) = &inner.db {
            parts.push(db.to_string());
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
            None => self.0.db.as_ref().map(|db| db as _),
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match (&e.0.source, e.kind()) {
            (Some(source), _) => source.kind(),
            (None, ErrorKind::Config | ErrorKind::Statement) => io::ErrorKind::InvalidInput,
            (None, ErrorKind::Auth) => io::ErrorKind::PermissionDenied,
            (None, ErrorKind::Type | ErrorKind::Protocol) => io::ErrorKind::InvalidData,
            (None, ErrorKind::PoolTimeout) => io::ErrorKind::TimedOut,
            (None, ErrorKind::Closed) => io::ErrorKind::ConnectionAborted,
            (None, _) => io::ErrorKind::Other,
        };
        io::Error::new(kind, giong::error::Error::wrap(e.giong_code(), e))
    }
}

/// An error reported by the server: its number, SQLSTATE and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    code: u16,
    sqlstate: String,
    message: String,
}

impl DbError {
    pub(crate) fn new(code: u16, sqlstate: &str, message: &str) -> Self {
        Self {
            code,
            sqlstate: sqlstate.to_owned(),
            message: message.to_owned(),
        }
    }

    /// Reads an `ERR_Packet`.
    pub(crate) fn parse(payload: &[u8]) -> Result<Self, Error> {
        let e = crate::protocol::err(payload)?;
        Ok(Self::new(e.code, e.sqlstate, e.message))
    }

    /// MySQL's error number, such as 1062 (`ER_DUP_ENTRY`) or 1213
    /// (`ER_LOCK_DEADLOCK`).
    pub fn code(&self) -> u16 {
        self.code
    }

    /// The SQLSTATE, such as `"23000"` for an integrity constraint violation.
    pub fn sqlstate(&self) -> &str {
        &self.sqlstate
    }

    /// The message.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ERROR {} ({}): {}",
            self.code, self.sqlstate, self.message
        )
    }
}

impl std::error::Error for DbError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_with_its_giong_code_and_link() {
        let e = Error::with_db(
            ErrorKind::Auth,
            "the server refused the login",
            DbError::new(1045, "28000", "Access denied for user 'app'@'localhost'"),
        );
        assert_eq!(
            e.to_string(),
            "GIONG-E3103: the server refused the login: ERROR 1045 (28000): Access denied for \
             user 'app'@'localhost'. See \
             https://github.com/giong-run/Giong/blob/main/docs/errors/E3103.md"
        );
        assert_eq!(e.giong_code(), Code::MysqlAuth);
        assert_eq!(e.code(), Some(1045));
    }

    #[test]
    fn the_code_and_the_error_survive_the_conversion_to_io() {
        let e = Error::new(ErrorKind::PoolTimeout, "no connection became free");
        let shown = e.to_string();
        let io: io::Error = e.into();
        assert_eq!(io.kind(), io::ErrorKind::TimedOut);
        assert_eq!(io.to_string(), shown);
        assert_eq!(giong::error::code(&io), Some(Code::MysqlPoolTimeout));
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
            ErrorKind::Db,
            ErrorKind::Type,
            ErrorKind::RowCount,
            ErrorKind::TooLarge,
            ErrorKind::Closed,
            ErrorKind::PoolTimeout,
            ErrorKind::Protocol,
            ErrorKind::Statement,
        ];
        let codes: std::collections::HashSet<_> = kinds.iter().map(|k| k.giong_code()).collect();
        assert_eq!(codes.len(), kinds.len());
        assert!(codes.iter().all(|c| (3101..3200).contains(&c.number())));
    }
}
