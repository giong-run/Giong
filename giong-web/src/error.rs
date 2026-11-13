//! The error a handler or a middleware returns, and the response it becomes.

use std::borrow::Cow;
use std::cell::Cell;
use std::error::Error as StdError;
use std::fmt;
use std::io;

use giong::error::Code;
use giong_http::Response;

use crate::json::JsonWriter;
use crate::req::IdBuf;

/// Why a request failed, and how it is answered.
///
/// Handlers and middleware return `Result<Response, Error>`, so `?` works on
/// every error: any type that implements [`std::error::Error`] converts into
/// an `Error` with status `500` (code `GIONG-E1210`), or `503` (code
/// `GIONG-E1211`) when the error says a database, cache or upstream API could
/// not be reached in time. Their message goes to the log, never to the client.
/// The errors of a request body read as a stream are the client's doing:
/// `413` (`GIONG-E1111`) past the route's limit, `400` (`GIONG-E1112`,
/// `GIONG-E1113`) cut short or malformed.
///
/// Errors meant for the client are made with a status, a stable code the
/// client can match on, and a detail that says what to do:
///
/// ```
/// use giong_web::{Error, Req, Response};
///
/// async fn show(req: Req) -> Result<Response, Error> {
///     let id: u64 = req.param("id")?; // `400` with GIONG-E1205 if it is not a number
///     if id == 0 {
///         return Err(Error::not_found("user_not_found", "no user has id 0"));
///     }
///     Ok(Response::text(format!("user {id}")))
/// }
/// ```
///
/// The client gets a JSON body in the form of RFC 9457 ("problem details"),
/// with `Content-Type: application/problem+json`:
///
/// ```json
/// {"status":404,"code":"user_not_found","title":"Not Found","detail":"no user has id 0","request_id":"5f3a9c2e01000000002a"}
/// ```
///
/// `request_id` is there when the [`RequestId`](crate::middleware::RequestId)
/// middleware runs.
///
/// `Error` does not implement [`std::error::Error`] itself: that is what lets
/// every other error convert into it with `?`.
pub struct Error {
    status: u16,
    code: Cow<'static, str>,
    detail: Cow<'static, str>,
    source: Option<Box<dyn StdError + Send + Sync>>,
    headers: Vec<(Cow<'static, str>, Cow<'static, str>)>,
}

/// What [`Error::internal`] and [`Error::unavailable`] tell the client.
const INTERNAL_DETAIL: &str = "the server failed to answer this request; the cause is in its log";
const UNAVAILABLE_DETAIL: &str =
    "a service this request needs is unavailable or overloaded; try again shortly";

impl Error {
    /// An error answered with `status`, `code` and `detail`.
    ///
    /// `code` is a short, stable name the client can match on, such as
    /// `"user_not_found"`; `detail` says, for a person or a program, what went
    /// wrong and what to do about it. Both are sent to the client.
    ///
    /// # Panics
    ///
    /// Panics if `status` is not an error status (`400..=599`).
    #[track_caller]
    pub fn new(
        status: u16,
        code: impl Into<Cow<'static, str>>,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        assert!(
            (400..=599).contains(&status),
            "giong-web: an Error needs a status in 400..=599, not {status}"
        );
        Self {
            status,
            code: code.into(),
            detail: detail.into(),
            source: None,
            headers: Vec::new(),
        }
    }

    /// `400 Bad Request`: the request is malformed.
    pub fn bad_request(
        code: impl Into<Cow<'static, str>>,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(400, code, detail)
    }

    /// `401 Unauthorized`: the request lacks valid credentials. Add a
    /// `WWW-Authenticate` header with [`with_header`](Self::with_header).
    pub fn unauthorized(
        code: impl Into<Cow<'static, str>>,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(401, code, detail)
    }

    /// `403 Forbidden`: the credentials are valid but not enough.
    pub fn forbidden(
        code: impl Into<Cow<'static, str>>,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(403, code, detail)
    }

    /// `404 Not Found`.
    pub fn not_found(
        code: impl Into<Cow<'static, str>>,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(404, code, detail)
    }

    /// `409 Conflict`: the request conflicts with the current state, such as
    /// a duplicate key.
    pub fn conflict(
        code: impl Into<Cow<'static, str>>,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(409, code, detail)
    }

    /// `422 Unprocessable Content`: the request is well formed but its
    /// content is not acceptable, such as a failed validation.
    pub fn unprocessable(
        code: impl Into<Cow<'static, str>>,
        detail: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::new(422, code, detail)
    }

    /// `500 Internal Server Error` caused by `cause`, with code
    /// `GIONG-E1210`. The client gets a generic detail; `cause` goes to the
    /// log.
    pub fn internal(cause: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self::coded(500, Code::WebInternal, INTERNAL_DETAIL).with_source(cause)
    }

    /// `503 Service Unavailable` caused by `cause`, with code `GIONG-E1211`
    /// and `Retry-After: 1`: a database, cache or upstream service is
    /// unreachable or overloaded. The client gets a generic detail; `cause`
    /// goes to the log.
    pub fn unavailable(cause: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        Self::coded(503, Code::WebUnavailable, UNAVAILABLE_DETAIL)
            .with_header("Retry-After", "1")
            .with_source(cause)
    }

    /// An error with one of giong's own codes.
    pub(crate) fn coded(status: u16, code: Code, detail: impl Into<Cow<'static, str>>) -> Self {
        Self::new(status, code.to_string(), detail)
    }

    /// Adds a header to the response, such as `WWW-Authenticate` or
    /// `Retry-After`.
    ///
    /// # Panics
    ///
    /// Panics if `name` is not a valid header name or `value` contains
    /// control characters, as [`Response::header`] does.
    #[track_caller]
    pub fn with_header(
        mut self,
        name: impl Into<Cow<'static, str>>,
        value: impl Into<Cow<'static, str>>,
    ) -> Self {
        let (name, value) = (name.into(), value.into());
        assert!(
            crate::header::is_token(&name),
            "giong-web: invalid header name {name:?}"
        );
        assert!(
            crate::header::is_value(&value),
            "giong-web: invalid value for header {name}: {value:?}"
        );
        self.headers.push((name, value));
        self
    }

    /// Sets the cause: shown in the log, never sent to the client.
    pub fn with_source(mut self, cause: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        self.source = Some(cause.into());
        self
    }

    /// The status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The code the client gets, such as `GIONG-E1205` or `user_not_found`.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The detail the client gets.
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// The cause, if there is one. Logged, never sent.
    pub fn source(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
        self.source.as_deref()
    }

    /// The error a response was made from, if it was made from one.
    ///
    /// Every `Error` a handler or middleware returns becomes a response that
    /// carries it, so middleware further out, such as a logger, can read the
    /// code and the cause.
    pub fn of(response: &Response) -> Option<&Error> {
        response.extension::<Rendered>().map(|r| &r.error)
    }

    /// The cause and its own causes, joined with `": "`.
    pub(crate) fn cause_chain(&self) -> Option<String> {
        let first = self.source.as_deref()?;
        let mut out = first.to_string();
        let mut next = first.source();
        while let Some(e) = next {
            out.push_str(": ");
            out.push_str(&e.to_string());
            next = e.source();
        }
        Some(out)
    }

    /// The response for this error: a problem-details JSON body.
    pub(crate) fn into_response(self, request_id: Option<IdBuf>) -> Response {
        let mut body = JsonWriter::object();
        body.u64("status", u64::from(self.status));
        body.str("code", &self.code);
        body.str("title", giong_http::reason_phrase(self.status));
        body.str("detail", &self.detail);
        if let Some(id) = &request_id {
            body.str("request_id", id.as_str());
        }
        let mut resp = Response::new(self.status)
            .header("Content-Type", "application/problem+json")
            .header("Cache-Control", "no-store");
        for (name, value) in &self.headers {
            resp = resp.header(name, value);
        }
        resp.body(body.finish().into_bytes())
            .with_extension(Rendered {
                error: self,
                request_id,
                logged: Cell::new(false),
            })
    }

    /// Turns a cause into an error, choosing `503` for the giong codes that
    /// mean "unreachable or overloaded" and `500` for everything else.
    fn from_cause(cause: Box<dyn StdError + Send + Sync>) -> Self {
        match giong_code(&*cause) {
            Some(code) if is_unavailable(code) => Self::unavailable(cause),
            // A streamed request body that went wrong: the client's doing.
            Some(code @ Code::HttpBodyTooLarge) => Self::coded(
                413,
                code,
                "the request body is larger than this route accepts",
            )
            .with_source(cause),
            Some(code @ Code::HttpBodyIncomplete) => {
                Self::coded(400, code, "the request body was cut short").with_source(cause)
            }
            Some(code @ Code::HttpBodyMalformed) => {
                Self::coded(400, code, "the request body's chunked framing is broken")
                    .with_source(cause)
            }
            _ => Self::internal(cause),
        }
    }
}

/// An [`Error`] carried by the response made from it, and whether a logger
/// has reported it yet.
pub(crate) struct Rendered {
    pub(crate) error: Error,
    pub(crate) request_id: Option<IdBuf>,
    pub(crate) logged: Cell<bool>,
}

impl<E: StdError + Send + Sync + 'static> From<E> for Error {
    fn from(error: E) -> Self {
        Self::from_cause(Box::new(error))
    }
}

/// The giong code of an error: from an `io::Error` giong raised, or from the
/// message of a companion crate's error, which starts with it.
fn giong_code(error: &(dyn StdError + Send + Sync + 'static)) -> Option<Code> {
    if let Some(io) = error.downcast_ref::<io::Error>() {
        if let Some(code) = giong::error::code(io) {
            return Some(code);
        }
    }
    let shown = error.to_string();
    let digits = shown.strip_prefix("GIONG-E")?.get(..4)?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Code::from_number(digits.parse().ok()?)
}

/// Codes that mean a dependency is unreachable, overloaded or slow, so the
/// same request may well succeed shortly: answered with `503`.
fn is_unavailable(code: Code) -> bool {
    matches!(
        code,
        Code::HttpClientConnect
            | Code::HttpClientTimeout
            | Code::PostgresConnect
            | Code::PostgresClosed
            | Code::PostgresPoolTimeout
            | Code::MysqlConnect
            | Code::MysqlClosed
            | Code::MysqlPoolTimeout
            | Code::RedisConnect
            | Code::RedisClosed
            | Code::RedisPoolTimeout
            | Code::RedisTimeout
    )
}

impl fmt::Display for Error {
    /// `404 user_not_found: no user has id 0`, followed by the cause.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.status, self.code, self.detail)?;
        if let Some(cause) = self.cause_chain() {
            write!(f, " (cause: {cause})")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("status", &self.status)
            .field("code", &self.code)
            .field("detail", &self.detail)
            .field("source", &self.source)
            .field("headers", &self.headers)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(resp: &Response) -> String {
        String::from_utf8(resp.body_bytes().to_vec()).unwrap()
    }

    #[test]
    fn a_client_error_is_answered_as_problem_details() {
        let resp = Error::not_found("user_not_found", "no user has id \"7\"")
            .with_header("X-Hint", "try /users")
            .into_response(IdBuf::new("abc"));
        assert_eq!(resp.status(), 404);
        assert_eq!(
            resp.get_header("Content-Type"),
            Some("application/problem+json")
        );
        assert_eq!(resp.get_header("X-Hint"), Some("try /users"));
        assert_eq!(
            body(&resp),
            r#"{"status":404,"code":"user_not_found","title":"Not Found","detail":"no user has id \"7\"","request_id":"abc"}"#
        );
        let carried = Error::of(&resp).unwrap();
        assert_eq!((carried.status(), carried.code()), (404, "user_not_found"));
    }

    #[test]
    fn a_cause_is_logged_but_never_sent() {
        let err: Error = io::Error::other("disk on fire").into();
        assert_eq!((err.status(), err.code()), (500, "GIONG-E1210"));
        assert_eq!(err.cause_chain().as_deref(), Some("disk on fire"));
        let resp = err.into_response(None);
        assert!(!body(&resp).contains("disk"));
        assert!(Error::of(&resp).unwrap().source().is_some());
    }

    #[test]
    fn unreachable_dependencies_are_503_with_retry_after() {
        let pool: io::Error = giong::error::Error::new(
            Code::PostgresPoolTimeout,
            "no connection became free within 5s",
            "raise the pool size.",
        )
        .into();
        let err: Error = pool.into();
        assert_eq!((err.status(), err.code()), (503, "GIONG-E1211"));
        let resp = err.into_response(None);
        assert_eq!(resp.get_header("Retry-After"), Some("1"));

        // A companion crate's own error type, recognised by its message.
        #[derive(Debug)]
        struct RedisDown;
        impl fmt::Display for RedisDown {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("GIONG-E3202: cannot connect to Redis at cache:6379")
            }
        }
        impl StdError for RedisDown {}
        assert_eq!(Error::from(RedisDown).status(), 503);

        let other: io::Error = giong::error::Error::new(Code::PostgresType, "x", "y").into();
        assert_eq!(Error::from(other).status(), 500);
    }

    #[test]
    fn display_shows_status_code_detail_and_cause() {
        let err = Error::bad_request("bad", "no").with_source(io::Error::other("why"));
        assert_eq!(err.to_string(), "400 bad: no (cause: why)");
    }

    #[test]
    #[should_panic(expected = "status in 400..=599")]
    fn success_statuses_are_not_errors() {
        let _ = Error::new(200, "ok", "fine");
    }

    #[test]
    #[should_panic(expected = "invalid value for header")]
    fn headers_cannot_split_the_response() {
        let _ = Error::bad_request("x", "y").with_header("X-A", "1\r\nSet-Cookie: a=b");
    }
}
