//! The request a handler gets: giong-http's request, plus what routing found.

use std::any::Any;
use std::borrow::Cow;
use std::fmt;
use std::net::SocketAddr;
use std::rc::Rc;
use std::str::FromStr;

use giong::error::Code;
use giong_http::Request;

use crate::percent;
use crate::route::Captures;
use crate::Error;

/// What a route knows about itself: its pattern and parameter names.
#[derive(Debug)]
pub(crate) struct RouteInfo {
    pub(crate) pattern: Box<str>,
    pub(crate) names: Box<[Box<str>]>,
    /// The last name is a catch-all, whose value may span segments.
    pub(crate) catch_all: bool,
}

/// Longest request id kept.
pub(crate) const MAX_ID: usize = 64;

/// A request id, stored inline so that copying it costs nothing.
#[derive(Clone, Copy)]
pub(crate) struct IdBuf {
    len: u8,
    bytes: [u8; MAX_ID],
}

impl IdBuf {
    /// `None` unless `id` is 1 to 64 characters from `A-Z a-z 0-9 - _ . : /
    /// + =`.
    pub(crate) fn new(id: &str) -> Option<IdBuf> {
        let ok = !id.is_empty()
            && id.len() <= MAX_ID
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:/+=".contains(&b));
        ok.then(|| {
            let mut bytes = [0; MAX_ID];
            bytes[..id.len()].copy_from_slice(id.as_bytes());
            IdBuf {
                len: id.len() as u8,
                bytes,
            }
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        // Only ASCII is ever stored.
        std::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

/// An HTTP request, as a giong-web handler sees it.
///
/// It holds giong-http's [`Request`] (method, path, headers, body), the
/// router's state `S`, the route that matched and its path parameters.
///
/// ```
/// use giong_web::{Error, Req, Response};
///
/// struct App {
///     greeting: String,
/// }
///
/// // GET /hello/{name}?times=3
/// async fn hello(req: Req<App>) -> Result<Response, Error> {
///     let name: String = req.param("name")?;
///     let times = req.query_param::<usize>("times")?.unwrap_or(1);
///     let line = format!("{}, {name}!\n", req.state().greeting);
///     Ok(Response::text(line.repeat(times.min(10))))
/// }
/// ```
pub struct Req<S = ()> {
    http: Request,
    state: Rc<S>,
    route: Option<Rc<RouteInfo>>,
    caps: Captures,
    id: Option<IdBuf>,
    extensions: Vec<Box<dyn Any>>,
}

impl<S> Req<S> {
    pub(crate) fn new(
        http: Request,
        state: Rc<S>,
        route: Option<Rc<RouteInfo>>,
        caps: Captures,
    ) -> Self {
        Self {
            http,
            state,
            route,
            caps,
            id: None,
            extensions: Vec::new(),
        }
    }

    /// The state given to [`Router::build`](crate::Router::build).
    pub fn state(&self) -> &S {
        &self.state
    }

    /// The state, shared: for a task spawned from the handler, which
    /// cannot borrow from the request.
    pub fn state_rc(&self) -> Rc<S> {
        self.state.clone()
    }

    /// The path parameter `name`, percent-decoded and parsed as `T`.
    ///
    /// For the route `/users/{id}` and the path `/users/42`,
    /// `req.param::<u64>("id")` is `Ok(42)`. `T` is any [`FromStr`] type:
    /// `String`, the integers, `bool`, `Uuid` from the `uuid` crate...
    ///
    /// # Values that could leave a directory
    ///
    /// The value is refused, with a `400`, when once decoded it could name
    /// a file outside the directory a handler joins it to, or end a C
    /// string early:
    ///
    /// * a `.` or `..` segment, whether sent as is (`/files/../x`, which
    ///   giong-http passes on unchanged) or encoded (`%2E%2E`, `..%2F`);
    /// * a NUL character (`%00`);
    /// * a `/` (`%2F`) in a one-segment parameter (`{name}`), which would
    ///   make it several segments;
    /// * a catch-all (`{*path}`) value that starts with `/` (`/files//etc`),
    ///   which `Path::join` would take as an absolute path.
    ///
    /// So a catch-all can be joined to a directory as it is:
    ///
    /// ```
    /// use std::path::Path;
    /// use giong_web::{Error, Req, Response};
    ///
    /// // GET /files/{*path}
    /// async fn file(req: Req) -> Result<Response, Error> {
    ///     // Never `..`, `.`, NUL or absolute: it stays under /srv/files
    ///     // (unless a symbolic link in there leads out).
    ///     let path: String = req.param("path")?;
    ///     let full = Path::new("/srv/files").join(&path);
    ///     # let _ = full;
    ///     # Ok(Response::new(204))
    /// }
    /// ```
    ///
    /// A route that wants such values reads [`param_raw`](Self::param_raw)
    /// instead and decodes and checks them itself.
    ///
    /// # Errors
    ///
    /// * `400` (`GIONG-E1205`) when the value is not a valid `T`, is not
    ///   valid percent-encoded UTF-8, or is one of the values above. The
    ///   detail names the parameter.
    /// * `500` (`GIONG-E1206`) when the route has no parameter `name`: a bug
    ///   in the handler, such as a misspelt name.
    pub fn param<T>(&self, name: &str) -> Result<T, Error>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        let (raw, catch_all) = self.raw_param(name)?;
        let bad = |why: &str| {
            Error::coded(
                400,
                Code::WebBadParameter,
                format!("path parameter `{name}`: {why}"),
            )
        };
        let text = percent::decode(raw).map_err(|e| bad(e.describe()))?;
        check_path_value(&text, catch_all).map_err(bad)?;
        parse_value(&text).map_err(|why| {
            Error::coded(
                400,
                Code::WebBadParameter,
                format!(
                    "path parameter `{name}`: {why}; expected {}",
                    friendly_type::<T>()
                ),
            )
        })
    }

    /// The path parameter `name` exactly as it appears in the path: still
    /// percent-encoded, and without the checks of [`param`](Self::param),
    /// so it may decode to `..`, a NUL, or a `/` in a one-segment parameter.
    ///
    /// For the rare route that wants such values, such as one that passes
    /// a key on to a store where `/` and `..` mean nothing special. Decode
    /// it with a percent-decoding crate, and never join it to a path.
    ///
    /// # Errors
    ///
    /// `500` (`GIONG-E1206`) when the route has no parameter `name`, as for
    /// [`param`](Self::param).
    pub fn param_raw(&self, name: &str) -> Result<&str, Error> {
        self.raw_param(name).map(|(raw, _)| raw)
    }

    /// The path parameter `name` as it appears in the path, and whether it
    /// is a catch-all.
    fn raw_param(&self, name: &str) -> Result<(&str, bool), Error> {
        let route = self.route.as_deref();
        let index = route.and_then(|r| r.names.iter().position(|n| &**n == name));
        let span = index.and_then(|i| self.caps.get(i));
        match (span, route, index) {
            (Some((start, end)), Some(r), Some(i)) => {
                let catch_all = r.catch_all && i + 1 == r.names.len();
                Ok((&self.http.path()[start..end], catch_all))
            }
            _ => {
                let pattern = route.map_or("(no route)", |r| &r.pattern);
                Err(Error::coded(
                    500,
                    Code::WebNoSuchParameter,
                    "the server's route has a bug; the server's log says which",
                )
                .with_source(format!(
                    "the route {pattern} has no parameter `{name}`; use one of the names in \
                     braces in the pattern"
                )))
            }
        }
    }

    /// The query-string parameter `name`, percent-decoded (`+` is a space)
    /// and parsed as `T`; `None` when the query string does not have it.
    ///
    /// ```
    /// # use giong_web::{Error, Req, Response};
    /// // GET /users?page=2
    /// async fn list(req: Req) -> Result<Response, Error> {
    ///     let page: u32 = req.query_param("page")?.unwrap_or(1);
    ///     # let _ = page;
    ///     # Ok(Response::new(204))
    /// }
    /// ```
    ///
    /// When the key appears more than once, the first value counts.
    ///
    /// # Errors
    ///
    /// `400` (`GIONG-E1205`) when the value is not a valid `T` or the query
    /// string is not valid percent-encoded UTF-8 where the key or value is.
    pub fn query_param<T>(&self, name: &str) -> Result<Option<T>, Error>
    where
        T: FromStr,
        T::Err: fmt::Display,
    {
        let bad = |why: String| {
            Error::coded(
                400,
                Code::WebBadParameter,
                format!("query parameter `{name}`: {why}"),
            )
        };
        for (key, value) in percent::query_pairs(self.http.query().unwrap_or("")) {
            let key = match percent::decode_query(key) {
                Ok(key) => key,
                Err(_) => continue,
            };
            if key != name {
                continue;
            }
            let value = percent::decode_query(value).map_err(|e| bad(e.describe().into()))?;
            return parse_value(&value)
                .map(Some)
                .map_err(|why| bad(format!("{why}; expected {}", friendly_type::<T>())));
        }
        Ok(None)
    }

    /// The pattern of the route that matched, such as `/users/{id}`. `None`
    /// when no route matched (the request is being answered with `404` or
    /// `405`).
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref().map(|r| &*r.pattern)
    }

    /// The request id, once the [`RequestId`](crate::middleware::RequestId)
    /// middleware or [`set_request_id`](Self::set_request_id) has set one.
    pub fn request_id(&self) -> Option<&str> {
        self.id.as_ref().map(IdBuf::as_str)
    }

    pub(crate) fn id_buf(&self) -> Option<IdBuf> {
        self.id
    }

    pub(crate) fn set_id_buf(&mut self, id: IdBuf) {
        self.id = Some(id);
    }

    /// Sets the request id, which error responses and the
    /// [`Logger`](crate::middleware::Logger) show.
    ///
    /// Returns `false`, and keeps the old id, unless `id` is 1 to 64
    /// characters from `A-Z`, `a-z`, `0-9` and `-_.:/+=`: an id taken from
    /// a client's header is checked before it reaches a log.
    pub fn set_request_id(&mut self, id: &str) -> bool {
        match IdBuf::new(id) {
            Some(id) => {
                self.id = Some(id);
                true
            }
            None => false,
        }
    }

    /// Attaches `value` for the code further in (a middleware passing the
    /// signed-in user to handlers, say), replacing a value of the same type.
    pub fn set_extension<T: 'static>(&mut self, value: T) {
        match self.extensions.iter_mut().find(|v| v.is::<T>()) {
            Some(slot) => *slot = Box::new(value),
            None => self.extensions.push(Box::new(value)),
        }
    }

    /// The value of type `T` attached with
    /// [`set_extension`](Self::set_extension).
    pub fn extension<T: 'static>(&self) -> Option<&T> {
        self.extensions.iter().find_map(|v| v.downcast_ref())
    }

    /// The request method, such as `GET`.
    pub fn method(&self) -> &str {
        self.http.method()
    }

    /// The path, without the query string, still percent-encoded.
    pub fn path(&self) -> &str {
        self.http.path()
    }

    /// The query string without the `?`, still encoded.
    pub fn query_string(&self) -> Option<&str> {
        self.http.query()
    }

    /// The first value of the header `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.http.header(name)
    }

    /// Like [`header`](Self::header), for values that are valid UTF-8.
    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.http.header_str(name)
    }

    /// The request body, received in full before the handler runs (within
    /// the server's `max_body_size`). Empty on a route that reads its body
    /// as a stream: see [`body_stream`](Self::body_stream).
    pub fn body(&self) -> &[u8] {
        self.http.body()
    }

    /// The request body as it arrives, on a route of a router with
    /// [`Router::stream_bodies`](crate::Router::stream_bodies); `None` on
    /// other routes, whose body is in [`body`](Self::body). See
    /// [`BodyStream`](crate::BodyStream).
    pub fn body_stream(&self) -> Option<giong_http::BodyStream> {
        self.http.body_stream()
    }

    /// The client's address; `None` over a Unix socket.
    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.http.peer_addr()
    }

    /// giong-http's request, for everything else: every header, the target
    /// as sent, the HTTP version.
    pub fn http(&self) -> &Request {
        &self.http
    }

    /// giong-http's request, taken out.
    pub fn into_http(self) -> Request {
        self.http
    }
}

/// Why a percent-decoded path parameter is refused, if it is: a value that
/// a handler joining it to a directory would take outside that directory.
/// See [`Req::param`].
fn check_path_value(text: &str, catch_all: bool) -> Result<(), &'static str> {
    // Most values have none of these: one scan, and done.
    if !text.bytes().any(|b| matches!(b, b'.' | b'/' | 0)) {
        return Ok(());
    }
    if text.contains('\0') {
        return Err("a NUL character (%00) is not allowed");
    }
    if !catch_all && text.contains('/') {
        return Err("a `/` (%2F) is not allowed in a parameter of one segment");
    }
    if text.starts_with('/') {
        return Err("the value starts with `/`, which would make it an absolute path");
    }
    for segment in text.split('/') {
        match segment {
            "." => return Err("a `.` segment is not allowed"),
            ".." => return Err("a `..` segment is not allowed"),
            _ => {}
        }
    }
    Ok(())
}

/// Parses a parameter value, with the parser's own words for the failure.
fn parse_value<T>(text: &str) -> Result<T, String>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    text.parse::<T>()
        .map_err(|e| format!("cannot read {:?} ({e})", Shown(text)))
}

/// A value from the client, cut short in error details.
struct Shown<'a>(&'a str);

impl fmt::Debug for Shown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const MAX: usize = 64;
        if self.0.len() <= MAX {
            return fmt::Debug::fmt(self.0, f);
        }
        let mut end = MAX;
        while !self.0.is_char_boundary(end) {
            end -= 1;
        }
        write!(f, "{:?}...", &self.0[..end])
    }
}

/// `an unsigned integer` for `u64`, and so on: what the client should send.
fn friendly_type<T>() -> Cow<'static, str> {
    let name = std::any::type_name::<T>();
    Cow::Borrowed(match name {
        "u8" | "u16" | "u32" | "u64" | "u128" | "usize" => "an unsigned integer",
        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" => "an integer",
        "f32" | "f64" => "a number",
        "bool" => "`true` or `false`",
        "char" => "one character",
        "alloc::string::String" => "text",
        other => return Cow::Owned(format!("a value of type `{other}`")),
    })
}

impl<S> fmt::Debug for Req<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Req")
            .field("method", &self.method())
            .field("target", &self.http.target())
            .field("route", &self.route())
            .field("request_id", &self.request_id())
            .field("body_len", &self.body().len())
            .finish()
    }
}
