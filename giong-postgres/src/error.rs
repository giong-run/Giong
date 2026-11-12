//! Errors: what went wrong, and what to do about it.

use std::fmt;
use std::io;
use std::sync::Arc;

/// An error from giong-postgres.
///
/// Every error says what happened and, where there is one, what to do about
/// it. [`kind`](Self::kind) tells errors apart in code; errors the server
/// reported carry its fields in [`db`](Self::db), with the SQLSTATE code in
/// [`code`](Self::code).
///
/// Errors are cheap to clone, and convert into [`std::io::Error`], so `?`
/// works in functions that return `io::Result`.
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
    /// A value could not be converted between Rust and PostgreSQL: a
    /// parameter, or a column read with [`Row::get`](crate::Row::get).
    Type,
    /// [`query_one`](crate::Client::query_one) found no row or more than one.
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
                "{}; the connection was closed (is this really a PostgreSQL server?)",
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

    /// The error the server reported, for [`ErrorKind::Db`] (and for
    /// [`ErrorKind::Auth`] when the server refused the credentials).
    pub fn db(&self) -> Option<&DbError> {
        self.0.db.as_ref()
    }

    /// The SQLSTATE code of the server's error, such as `"23505"` for a
    /// unique violation. The codes are listed in the PostgreSQL manual,
    /// appendix A.
    pub fn code(&self) -> Option<&str> {
        self.0.db.as_ref().map(|db| db.code())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = &self.0;
        let mut wrote = false;
        if !inner.message.is_empty() {
            f.write_str(&inner.message)?;
            wrote = true;
        }
        if let Some(db) = &inner.db {
            if wrote {
                f.write_str(": ")?;
            }
            write!(f, "{db}")?;
            wrote = true;
        }
        if let Some(source) = &inner.source {
            if wrote {
                f.write_str(": ")?;
            }
            write!(f, "{source}")?;
        }
        Ok(())
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
            (None, ErrorKind::Config) => io::ErrorKind::InvalidInput,
            (None, ErrorKind::Auth) => io::ErrorKind::PermissionDenied,
            (None, ErrorKind::Type | ErrorKind::Protocol) => io::ErrorKind::InvalidData,
            (None, ErrorKind::PoolTimeout) => io::ErrorKind::TimedOut,
            (None, ErrorKind::Closed) => io::ErrorKind::ConnectionAborted,
            (None, _) => io::ErrorKind::Other,
        };
        io::Error::new(kind, e)
    }
}

/// An error reported by the server, with the fields of its `ErrorResponse`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DbError {
    severity: String,
    code: String,
    message: String,
    detail: Option<String>,
    hint: Option<String>,
    position: Option<u32>,
    where_: Option<String>,
    schema: Option<String>,
    table: Option<String>,
    column: Option<String>,
    constraint: Option<String>,
}

impl DbError {
    /// Reads the fields of an `ErrorResponse` or `NoticeResponse` body.
    pub(crate) fn parse(body: &[u8]) -> Result<Self, Error> {
        let mut e = DbError {
            severity: String::new(),
            code: String::new(),
            message: String::new(),
            detail: None,
            hint: None,
            position: None,
            where_: None,
            schema: None,
            table: None,
            column: None,
            constraint: None,
        };
        let mut rest = body;
        loop {
            let (&field, tail) = rest
                .split_first()
                .ok_or_else(|| Error::protocol("an error message from the server is truncated"))?;
            if field == 0 {
                break;
            }
            let end = tail
                .iter()
                .position(|&b| b == 0)
                .ok_or_else(|| Error::protocol("an error message from the server is truncated"))?;
            let value = String::from_utf8_lossy(&tail[..end]).into_owned();
            rest = &tail[end + 1..];
            match field {
                b'V' => e.severity = value,
                b'S' if e.severity.is_empty() => e.severity = value,
                b'C' => e.code = value,
                b'M' => e.message = value,
                b'D' => e.detail = Some(value),
                b'H' => e.hint = Some(value),
                b'P' => e.position = value.parse().ok(),
                b'W' => e.where_ = Some(value),
                b's' => e.schema = Some(value),
                b't' => e.table = Some(value),
                b'c' => e.column = Some(value),
                b'n' => e.constraint = Some(value),
                _ => {}
            }
        }
        Ok(e)
    }

    /// `ERROR`, `FATAL` or `PANIC` (not localized).
    pub fn severity(&self) -> &str {
        &self.severity
    }

    /// The SQLSTATE code, such as `"23505"` (unique violation) or `"40001"`
    /// (serialization failure).
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The primary message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// More detail, when the server gave it.
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    /// A suggestion from the server about what to do.
    pub fn hint(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// Where in the SQL text the error is, as a 1-based character position.
    pub fn position(&self) -> Option<u32> {
        self.position
    }

    /// The call stack, for errors inside functions.
    pub fn where_(&self) -> Option<&str> {
        self.where_.as_deref()
    }

    /// The schema of the object the error is about.
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// The table the error is about.
    pub fn table(&self) -> Option<&str> {
        self.table.as_deref()
    }

    /// The column the error is about.
    pub fn column(&self) -> Option<&str> {
        self.column.as_deref()
    }

    /// The constraint that was violated.
    pub fn constraint(&self) -> Option<&str> {
        self.constraint.as_deref()
    }
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.severity, self.code, self.message)?;
        if let Some(detail) = &self.detail {
            write!(f, " (detail: {detail})")?;
        }
        if let Some(hint) = &self.hint {
            write!(f, " (hint: {hint})")?;
        }
        if let Some(position) = self.position {
            write!(f, " (at character {position})")?;
        }
        Ok(())
    }
}

impl std::error::Error for DbError {}
