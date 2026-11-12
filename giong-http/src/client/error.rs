//! The client's errors: what went wrong, and what to do about it.

use std::fmt;
use std::io;
use std::sync::Arc;

use giong::error::Code;

/// An error from the HTTP client.
///
/// [`kind`](Error::kind) says what went wrong; the message says where and how
/// to fix it, and starts with the error's giong code
/// (`GIONG-E1103: GET https://api.example.com/ did not finish within 30 s …`).
/// Cheap to clone. Converts into [`io::Error`], where
/// [`giong::error::code`] finds the code again.
#[derive(Clone)]
pub struct Error(Arc<Inner>);

struct Inner {
    kind: ErrorKind,
    message: String,
    status: Option<u16>,
    source: Option<io::Error>,
}

/// What kind of error an [`Error`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The URL, a header, the method or the client's settings are not valid.
    /// Nothing was sent.
    Request,
    /// Opening a connection failed: name resolution, TCP, TLS, or the
    /// connect timeout.
    Connect,
    /// The request did not finish within its timeout.
    Timeout,
    /// The response was larger than the client's limit.
    TooLarge,
    /// A redirect was not followed: too many of them, from HTTPS to HTTP,
    /// or a `Location` that is not a valid URL.
    Redirect,
    /// The connection closed before the whole response arrived.
    Closed,
    /// The server sent something that is not a valid HTTP/1.1 response.
    Protocol,
    /// [`Response::error_for_status`](super::Response::error_for_status)
    /// found a 4xx or 5xx status.
    Status,
    /// The body could not be read as asked: text that is not UTF-8, or a
    /// compressed body that is damaged or cut short.
    Decode,
    /// The proxy refused the tunnel to an `https` server (`407 Proxy
    /// Authentication Required`, `403`...) or did not answer as a proxy
    /// does. [`Error::status`] has its status, if it sent one.
    Proxy,
}

impl ErrorKind {
    /// The giong error code for this kind.
    pub fn giong_code(self) -> Code {
        match self {
            ErrorKind::Request => Code::HttpClientRequest,
            ErrorKind::Connect => Code::HttpClientConnect,
            ErrorKind::Timeout => Code::HttpClientTimeout,
            ErrorKind::TooLarge => Code::HttpClientTooLarge,
            ErrorKind::Redirect => Code::HttpClientRedirect,
            ErrorKind::Closed => Code::HttpClientClosed,
            ErrorKind::Protocol => Code::HttpClientProtocol,
            ErrorKind::Status => Code::HttpClientStatus,
            ErrorKind::Decode => Code::HttpClientDecode,
            ErrorKind::Proxy => Code::HttpClientProxy,
        }
    }
}

impl Error {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            status: None,
            source: None,
        }))
    }

    pub(crate) fn io(kind: ErrorKind, message: impl Into<String>, source: io::Error) -> Self {
        Self(Arc::new(Inner {
            kind,
            message: message.into(),
            status: None,
            source: Some(source),
        }))
    }

    pub(crate) fn from_status(status: u16, message: impl Into<String>) -> Self {
        Self(Arc::new(Inner {
            kind: ErrorKind::Status,
            message: message.into(),
            status: Some(status),
            source: None,
        }))
    }

    pub(crate) fn proxy(status: Option<u16>, message: impl Into<String>) -> Self {
        Self(Arc::new(Inner {
            kind: ErrorKind::Proxy,
            message: message.into(),
            status,
            source: None,
        }))
    }

    pub(crate) fn request(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Request, message)
    }

    /// What kind of error this is.
    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// The giong error code, shown at the start of the message.
    pub fn giong_code(&self) -> Code {
        self.0.kind.giong_code()
    }

    /// The response's status, for [`ErrorKind::Status`]; the proxy's, for
    /// [`ErrorKind::Proxy`].
    pub fn status(&self) -> Option<u16> {
        self.0.status
    }

    /// Whether sending the same request again may succeed: timeouts, failed
    /// connections and connections closed early. Whether it is safe to
    /// repeat (a `POST` may have been processed) is for the caller to say.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.0.kind,
            ErrorKind::Connect | ErrorKind::Timeout | ErrorKind::Closed
        ) || self
            .0
            .status
            .is_some_and(|s| s == 429 || s == 502 || s == 503 || s == 504)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.giong_code();
        let mut what = self.0.message.clone();
        if let Some(source) = &self.0.source {
            what = format!("{what}: {source}");
        }
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
        self.0.source.as_ref().map(|e| e as _)
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        let kind = match (&e.0.source, e.kind()) {
            (Some(source), _) => source.kind(),
            (None, ErrorKind::Request) => io::ErrorKind::InvalidInput,
            (None, ErrorKind::Timeout) => io::ErrorKind::TimedOut,
            (None, ErrorKind::Protocol | ErrorKind::Decode) => io::ErrorKind::InvalidData,
            (None, ErrorKind::Closed) => io::ErrorKind::ConnectionAborted,
            (None, _) => io::ErrorKind::Other,
        };
        io::Error::new(kind, giong::error::Error::wrap(e.giong_code(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shown_with_its_code_and_link() {
        let e = Error::new(
            ErrorKind::Timeout,
            "GET http://x/ did not finish within 1 s",
        );
        assert_eq!(
            e.to_string(),
            "GIONG-E1103: GET http://x/ did not finish within 1 s. See \
             https://github.com/giong-run/Giong/blob/main/docs/errors/E1103.md"
        );
        let io: io::Error = e.into();
        assert_eq!(io.kind(), io::ErrorKind::TimedOut);
        assert_eq!(giong::error::code(&io), Some(Code::HttpClientTimeout));
    }

    #[test]
    fn every_kind_has_its_own_code() {
        let kinds = [
            ErrorKind::Request,
            ErrorKind::Connect,
            ErrorKind::Timeout,
            ErrorKind::TooLarge,
            ErrorKind::Redirect,
            ErrorKind::Closed,
            ErrorKind::Protocol,
            ErrorKind::Status,
            ErrorKind::Decode,
            ErrorKind::Proxy,
        ];
        let mut codes: Vec<u16> = kinds.iter().map(|k| k.giong_code().number()).collect();
        codes.dedup();
        assert_eq!(codes, (1101..=1110).collect::<Vec<_>>());
    }
}
