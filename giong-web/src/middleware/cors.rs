//! [`Cors`]: cross-origin resource sharing for browsers.

use std::time::Duration;

use giong::error::Code;

use crate::header::list_contains;
use crate::{Error, Middleware, Next, Req, Response};

#[derive(Clone, Debug)]
enum Origins {
    List(Vec<Box<str>>),
    Any,
}

/// Lets web pages from other origins call the service from a browser.
///
/// Nothing is allowed until it is named: [`allow_origin`](Self::allow_origin)
/// (or [`allow_any_origin`](Self::allow_any_origin)) for the pages, and
/// [`allow_methods`](Self::allow_methods) and
/// [`allow_headers`](Self::allow_headers) for what they may send beyond
/// `GET`, `HEAD`, `POST` and the always-safe headers.
///
/// ```
/// use std::time::Duration;
/// use giong_web::middleware::Cors;
///
/// let cors = Cors::new()
///     .allow_origin("https://app.example.com")
///     .allow_methods(&["GET", "POST", "PUT", "DELETE"])
///     .allow_headers(&["content-type", "authorization"])
///     .allow_credentials(true)
///     .max_age(Duration::from_secs(600));
/// # let _ = cors;
/// ```
///
/// A preflight request (`OPTIONS` with `Access-Control-Request-Method`) is
/// answered here, without reaching the routes: `204` with the
/// `Access-Control-Allow-*` headers when the origin, the method and every
/// requested header are allowed, `403` (`GIONG-E1212`) saying which one is
/// not otherwise. Other requests from an allowed origin get
/// `Access-Control-Allow-Origin` (and the credentials and exposed headers
/// settings) on their response; requests from other origins get none, so the
/// browser keeps the response from the page. Responses get `Vary: Origin`
/// whenever the answer depends on the origin.
///
/// It can be layered on the outermost router or on a nested one: a
/// preflight runs through the middleware of the route its
/// `Access-Control-Request-Method` would go to, and the `404` and `405`
/// answers under a nested router's prefix through that router's middleware
/// (see [`Router::layer`](crate::Router::layer)). Layer it before any
/// middleware that checks credentials: browsers send preflights without
/// cookies or an `Authorization` header.
#[derive(Clone, Debug)]
pub struct Cors {
    origins: Origins,
    methods: Vec<Box<str>>,
    /// Lower case.
    headers: Vec<Box<str>>,
    expose: Vec<Box<str>>,
    credentials: bool,
    max_age: Option<Duration>,
}

impl Default for Cors {
    fn default() -> Self {
        Self::new()
    }
}

/// Request headers a page may always send; preflights never need to ask for
/// them.
const SAFELISTED: [&str; 3] = ["accept", "accept-language", "content-language"];

impl Cors {
    /// Allows nothing yet: no origin, the methods `GET`, `HEAD` and `POST`,
    /// no extra headers, no credentials, no `max-age`.
    pub fn new() -> Self {
        Self {
            origins: Origins::List(Vec::new()),
            methods: vec!["GET".into(), "HEAD".into(), "POST".into()],
            headers: Vec::new(),
            expose: Vec::new(),
            credentials: false,
            max_age: None,
        }
    }

    /// Allows pages from `origin`, written as the browser sends it:
    /// `scheme://host` or `scheme://host:port`, without a path or a trailing
    /// slash. Call it once per origin.
    ///
    /// # Panics
    ///
    /// Panics if `origin` is not in that form, is `null`, or
    /// [`allow_any_origin`](Self::allow_any_origin) was called
    /// (`GIONG-E1213`).
    #[track_caller]
    pub fn allow_origin(mut self, origin: &str) -> Self {
        if let Err(why) = check_origin(origin) {
            super::invalid_config(
                format!("Cors::allow_origin({origin:?}): {why}"),
                "write the origin as the browser sends it, such as \"https://app.example.com\" \
                 or \"http://localhost:5173\".",
            );
        }
        match &mut self.origins {
            Origins::List(list) => list.push(origin.to_ascii_lowercase().into()),
            Origins::Any => super::invalid_config(
                "Cors::allow_origin after allow_any_origin: every origin is already allowed".into(),
                "use either allow_any_origin or a list of allow_origin calls.",
            ),
        }
        self
    }

    /// Allows pages from every origin, with `Access-Control-Allow-Origin:
    /// *`. Fit for public APIs that use no cookies.
    ///
    /// # Panics
    ///
    /// Panics if credentials are allowed or origins were listed
    /// (`GIONG-E1213`): browsers refuse `*` with credentials, and allowing
    /// every origin with them would let any site act as the signed-in user.
    #[track_caller]
    pub fn allow_any_origin(mut self) -> Self {
        if self.credentials {
            any_with_credentials();
        }
        if matches!(&self.origins, Origins::List(list) if !list.is_empty()) {
            super::invalid_config(
                "Cors::allow_any_origin after allow_origin".into(),
                "use either allow_any_origin or a list of allow_origin calls.",
            );
        }
        self.origins = Origins::Any;
        self
    }

    /// The methods pages may use (default: `GET`, `HEAD`, `POST`). Replaces
    /// the list.
    ///
    /// # Panics
    ///
    /// Panics if a name is not a method name (`GIONG-E1213`).
    #[track_caller]
    pub fn allow_methods(mut self, methods: &[&str]) -> Self {
        for m in methods {
            if !crate::header::is_token(m) {
                super::invalid_config(
                    format!("Cors::allow_methods: {m:?} is not a method name"),
                    "list methods such as \"GET\" and \"DELETE\".",
                );
            }
        }
        self.methods = methods.iter().map(|&m| m.into()).collect();
        self
    }

    /// The request headers pages may send besides `Accept`,
    /// `Accept-Language` and `Content-Language`, such as `content-type`
    /// (for JSON bodies) or `authorization`. Replaces the list.
    ///
    /// # Panics
    ///
    /// Panics if a name is not a header name (`GIONG-E1213`).
    #[track_caller]
    pub fn allow_headers(mut self, headers: &[&str]) -> Self {
        self.headers = header_list("allow_headers", headers);
        self
    }

    /// The response headers pages may read besides the always-readable ones
    /// (`Content-Type`, `Cache-Control`...), such as `x-request-id`.
    /// Replaces the list.
    ///
    /// # Panics
    ///
    /// Panics if a name is not a header name (`GIONG-E1213`).
    #[track_caller]
    pub fn expose_headers(mut self, headers: &[&str]) -> Self {
        self.expose = header_list("expose_headers", headers);
        self
    }

    /// Lets pages send cookies and HTTP authentication with their requests
    /// and read the responses (default: `false`).
    ///
    /// # Panics
    ///
    /// Panics if every origin is allowed (`GIONG-E1213`); see
    /// [`allow_any_origin`](Self::allow_any_origin).
    #[track_caller]
    pub fn allow_credentials(mut self, allow: bool) -> Self {
        if allow && matches!(self.origins, Origins::Any) {
            any_with_credentials();
        }
        self.credentials = allow;
        self
    }

    /// How long browsers may cache a preflight answer (default: not sent,
    /// so each browser uses its own default, 5 seconds in most).
    pub fn max_age(mut self, age: Duration) -> Self {
        self.max_age = Some(age);
        self
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        match &self.origins {
            Origins::Any => true,
            Origins::List(list) => list.iter().any(|o| o.eq_ignore_ascii_case(origin)),
        }
    }

    /// `Access-Control-Allow-Origin` and, when the answer depends on the
    /// origin, `Vary: Origin`.
    fn allow_origin_headers(&self, resp: Response, origin: &str) -> Response {
        match self.origins {
            Origins::Any => resp.header("Access-Control-Allow-Origin", "*"),
            Origins::List(_) => vary(resp, "Origin").header("Access-Control-Allow-Origin", origin),
        }
    }

    fn preflight(&self, req: &Req<impl Sized>, origin: &str) -> Result<Response, Error> {
        let refuse = |why: String| {
            Err(Error::coded(
                403,
                Code::WebCorsRejected,
                format!("CORS preflight refused: {why}"),
            ))
        };
        if !self.origin_allowed(origin) {
            return refuse(format!("the origin {origin} is not allowed"));
        }
        let method = req
            .header_str("access-control-request-method")
            .unwrap_or("")
            .trim();
        if !self.methods.iter().any(|m| &**m == method) {
            return refuse(format!(
                "the method {method} is not allowed; allowed: {}",
                self.methods.join(", ")
            ));
        }
        let asked = req
            .header_str("access-control-request-headers")
            .unwrap_or("");
        for name in asked.split(',').map(str::trim).filter(|h| !h.is_empty()) {
            let lower = name.to_ascii_lowercase();
            if !SAFELISTED.contains(&lower.as_str()) && !self.headers.iter().any(|h| **h == lower) {
                return refuse(format!(
                    "the header {name} is not allowed; allowed: {}",
                    if self.headers.is_empty() {
                        "none".to_string()
                    } else {
                        self.headers.join(", ")
                    }
                ));
            }
        }
        let mut resp = self.allow_origin_headers(Response::new(204), origin);
        resp = vary(resp, "Access-Control-Request-Method");
        resp = vary(resp, "Access-Control-Request-Headers");
        resp = resp.header("Access-Control-Allow-Methods", &self.methods.join(", "));
        if !self.headers.is_empty() {
            resp = resp.header("Access-Control-Allow-Headers", &self.headers.join(", "));
        }
        if self.credentials {
            resp = resp.header("Access-Control-Allow-Credentials", "true");
        }
        if let Some(age) = self.max_age {
            resp = resp.header("Access-Control-Max-Age", &age.as_secs().to_string());
        }
        Ok(resp)
    }
}

impl<S: 'static> Middleware<S> for Cors {
    async fn call(&self, req: Req<S>, next: Next<'_, S>) -> Result<Response, Error> {
        let Some(origin) = req.header_str("origin").map(str::to_owned) else {
            let resp = next.run(req).await;
            return Ok(match self.origins {
                Origins::List(_) => vary(resp, "Origin"),
                Origins::Any => resp,
            });
        };
        if req.method() == "OPTIONS" && req.header("access-control-request-method").is_some() {
            return self.preflight(&req, &origin);
        }
        let resp = next.run(req).await;
        if !self.origin_allowed(&origin) {
            return Ok(vary(resp, "Origin"));
        }
        let mut resp = self.allow_origin_headers(resp, &origin);
        if self.credentials {
            resp = resp.header("Access-Control-Allow-Credentials", "true");
        }
        if !self.expose.is_empty() {
            resp = resp.header("Access-Control-Expose-Headers", &self.expose.join(", "));
        }
        Ok(resp)
    }
}

/// Adds `item` to the response's `Vary`, unless it is there already.
pub(crate) fn vary(resp: Response, item: &str) -> Response {
    let present = resp.headers().any(|(n, v)| {
        n.eq_ignore_ascii_case("vary") && (list_contains(v, item) || v.trim() == "*")
    });
    if present {
        resp
    } else {
        resp.header("Vary", item)
    }
}

#[track_caller]
fn any_with_credentials() -> ! {
    super::invalid_config(
        "Cors: allow_any_origin together with allow_credentials(true)".into(),
        "list the origins that may send credentials with allow_origin.",
    )
}

#[track_caller]
fn header_list(what: &str, headers: &[&str]) -> Vec<Box<str>> {
    headers
        .iter()
        .map(|&h| {
            if !crate::header::is_token(h) {
                super::invalid_config(
                    format!("Cors::{what}: {h:?} is not a header name"),
                    "list header names such as \"content-type\".",
                );
            }
            h.to_ascii_lowercase().into()
        })
        .collect()
}

/// `scheme://host[:port]`, nothing more.
fn check_origin(origin: &str) -> Result<(), &'static str> {
    if origin.eq_ignore_ascii_case("null") {
        return Err("`null` is the origin of sandboxed and local pages, which any site can make");
    }
    let Some((scheme, rest)) = origin.split_once("://") else {
        return Err("it has no scheme (`https://`)");
    };
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    {
        return Err("the scheme is not valid");
    }
    if rest.is_empty() {
        return Err("it has no host");
    }
    if rest.contains(['/', '?', '#']) {
        return Err("an origin has no path, query or trailing slash");
    }
    if rest.contains(char::is_whitespace) || rest.contains('*') {
        return Err("the host is not valid (wildcards are not supported)");
    }
    Ok(())
}
