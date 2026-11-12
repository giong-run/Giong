//! An HTTP/1.1 client (feature `client`).
//!
//! ```no_run
//! use giong_http::client::Client;
//!
//! fn main() -> Result<(), giong_http::client::Error> {
//!     giong::block_on(async {
//!         let client = Client::new();
//!         let response = client
//!             .get("https://api.example.com/users/7")
//!             .header("accept", "application/json")
//!             .send()
//!             .await?
//!             .error_for_status()?;
//!         println!("{}", response.text()?);
//!         Ok(())
//!     })
//! }
//! ```
//!
//! A [`Client`] keeps connections open between requests, a pool for each
//! origin (scheme, host and port), and belongs to the worker that made it,
//! like everything in giong: make one per worker and share it between that
//! worker's tasks by cloning it (a clone shares the pool).
//!
//! What it does:
//!
//! * Keep-alive, up to [`max_connections_per_host`](ClientBuilder::max_connections_per_host)
//!   connections to each origin (16 by default); further requests wait
//!   for one. A connection the server closed while it sat idle is noticed
//!   before it is used, and an idempotent request that meets a connection
//!   closing under it is sent once more on a new one.
//! * HTTPS through giong-tls (rustls), with certificates checked against
//!   the machine's certificate authorities and the host name.
//! * Every request has a timeout (30 s by default) that covers waiting for
//!   a connection, connecting, sending, and reading the whole response,
//!   redirects included; connecting has its own (10 s).
//! * Redirects (301, 302, 303, 307, 308) are followed, up to 10. A redirect
//!   from HTTPS to HTTP is refused, and `Authorization` and `Cookie` headers
//!   are not sent to another origin.
//! * Responses with `Content-Length`, chunked, or ending with the
//!   connection; 1xx responses are skipped. The whole body is read, up to
//!   [`max_response_size`](ClientBuilder::max_response_size) (16 MiB by
//!   default).
//! * Strict: a malformed or ambiguous response (two lengths, bare line
//!   feeds, folded headers) is an error, and the connection is not used
//!   again.
//!
//! Not there yet: streaming bodies, compression, proxies, cookies, HTTP/2.

mod conn;
mod error;
mod head;
mod pool;
mod url;

use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use giong_tls::TlsConnector;

use self::conn::{Conn, Failure, Limits};
use self::url::{Origin, Url};
use crate::parse::{is_field_value, is_token_str};

pub use self::error::{Error, ErrorKind};

/// Checks for the randomised tests: they return whether the input was
/// accepted, and panic if a property does not hold.
#[cfg(test)]
pub(crate) mod fuzzing {
    use super::head::{parse_head, Framing};
    use super::url::Url;
    use crate::parse::{find_head_end, is_token_str};

    pub(crate) fn check_response_head(input: &[u8]) -> bool {
        let Some(end) = find_head_end(input, 0) else {
            return false;
        };
        let Ok(head) = parse_head(&input[..end]) else {
            return false;
        };
        assert_eq!(head.len, end);
        assert!((100..=599).contains(&head.status));
        for (name, value) in &head.fields {
            assert!(name.end <= end && value.end <= end && value.start >= name.end);
            assert!(is_token_str(&input[name.clone()]));
            assert!(!input[value.clone()]
                .iter()
                .any(|&b| b == b'\r' || b == b'\n'));
        }
        match head.framing(false) {
            Framing::Chunked => assert!(head.chunked && head.content_length.is_none()),
            Framing::Length(n) => assert_eq!(head.content_length, Some(n)),
            Framing::None | Framing::UntilClose => {}
        }
        true
    }

    pub(crate) fn check_url(input: &str, location: &str) -> bool {
        let Ok(url) = Url::parse(input) else {
            return false;
        };
        // What it prints, it reads back the same.
        assert_eq!(Url::parse(&url.to_string()), Ok(url.clone()), "{input:?}");
        assert!(url.target.starts_with('/'));
        if let Ok(next) = url.join(location) {
            assert!(next.target.starts_with('/'), "{input:?} + {location:?}");
            assert!(next.target.bytes().all(|b| (0x21..=0x7e).contains(&b)));
            assert!(!next.target.contains('#'));
        }
        true
    }
}

/// Headers the client sets itself, because they decide how the message is
/// framed or how the connection is used.
const MANAGED_HEADERS: &[&str] = &[
    "content-length",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "upgrade",
    "te",
    "trailer",
    "proxy-connection",
];

/// Headers that carry credentials, which are not sent to another origin
/// after a redirect.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "cookie", "proxy-authorization"];

/// Settings for a [`Client`]. Every limit is finite by default.
#[derive(Clone, Debug)]
pub struct ClientBuilder {
    timeout: Duration,
    connect_timeout: Duration,
    idle_timeout: Duration,
    max_connections_per_host: usize,
    max_response_size: usize,
    max_head_size: usize,
    redirects: usize,
    user_agent: String,
    tls: Option<TlsConnector>,
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(60),
            max_connections_per_host: 16,
            max_response_size: 16 * 1024 * 1024,
            max_head_size: 64 * 1024,
            redirects: 10,
            user_agent: concat!("giong-http/", env!("CARGO_PKG_VERSION")).to_owned(),
            tls: None,
        }
    }
}

impl ClientBuilder {
    /// How long a request may take in all (default 30 s): waiting for a
    /// connection, connecting, sending it, reading the whole response, and
    /// following redirects. [`RequestBuilder::timeout`] changes it for one
    /// request.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How long opening a connection may take, TLS included (default 10 s).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// How long an unused connection is kept open (default 60 s).
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// How many connections to one origin may be open at once (default 16,
    /// at least 1). Requests beyond that wait for one to be free.
    pub fn max_connections_per_host(mut self, n: usize) -> Self {
        self.max_connections_per_host = n.max(1);
        self
    }

    /// The largest response body accepted (default 16 MiB). A larger one
    /// fails with [`ErrorKind::TooLarge`].
    pub fn max_response_size(mut self, bytes: usize) -> Self {
        self.max_response_size = bytes;
        self
    }

    /// The largest response head accepted, status line and headers
    /// (default 64 KiB).
    pub fn max_head_size(mut self, bytes: usize) -> Self {
        self.max_head_size = bytes.max(64);
        self
    }

    /// How many redirects to follow (default 10); 0 returns 3xx responses
    /// as they are.
    pub fn redirects(mut self, n: usize) -> Self {
        self.redirects = n;
        self
    }

    /// The `User-Agent` header sent when a request has none (default
    /// `giong-http/<version>`).
    pub fn user_agent(mut self, user_agent: &str) -> Self {
        self.user_agent = user_agent.to_owned();
        self
    }

    /// The TLS settings for `https` URLs: other certificate authorities, a
    /// client certificate. By default, the machine's certificate
    /// authorities ([`TlsConnector::with_system_roots`]), loaded when the
    /// first `https` request is made.
    pub fn tls(mut self, connector: TlsConnector) -> Self {
        self.tls = Some(connector.alpn(&[b"http/1.1"]));
        self
    }

    /// The client.
    pub fn build(self) -> Client {
        Client {
            shared: Rc::new(Shared {
                tls: OnceCell::new(),
                config: self,
                hosts: RefCell::new(HashMap::new()),
            }),
        }
    }
}

/// An HTTP/1.1 client with a pool of connections for each origin.
///
/// Cheap to clone; clones share the pool. Not `Send`: make one per worker.
#[derive(Clone)]
pub struct Client {
    shared: Rc<Shared>,
}

struct Shared {
    config: ClientBuilder,
    tls: OnceCell<Result<TlsConnector, Error>>,
    hosts: RefCell<HashMap<Origin, pool::Shared>>,
}

/// A connection taken from the pool for one request. Dropped without
/// [`give_back`](Lease::give_back), it closes the connection and gives its
/// place to the next request in line.
struct Lease {
    conn: Option<Conn>,
    pool: pool::Shared,
    reused: bool,
}

impl Lease {
    fn conn(&mut self) -> &mut Conn {
        self.conn
            .as_mut()
            .expect("a lease holds its connection until it ends")
    }

    /// Puts the connection back for the next request.
    fn give_back(mut self, idle_timeout: Duration) {
        if let Some(conn) = self.conn.take() {
            pool::put_back(&self.pool, conn, idle_timeout);
        }
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            drop(conn);
            pool::free(&self.pool);
        }
    }
}

/// Holds a place in a pool while its connection is being opened.
struct Opening<'a> {
    pool: Option<&'a pool::Shared>,
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool::free(pool);
        }
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    /// A client with the default settings (see [`ClientBuilder`]).
    pub fn new() -> Self {
        ClientBuilder::default().build()
    }

    /// Settings for a new client.
    pub fn builder() -> ClientBuilder {
        ClientBuilder::default()
    }

    /// A `GET` request.
    pub fn get(&self, url: &str) -> RequestBuilder {
        self.request("GET", url)
    }

    /// A `POST` request.
    pub fn post(&self, url: &str) -> RequestBuilder {
        self.request("POST", url)
    }

    /// A `PUT` request.
    pub fn put(&self, url: &str) -> RequestBuilder {
        self.request("PUT", url)
    }

    /// A `PATCH` request.
    pub fn patch(&self, url: &str) -> RequestBuilder {
        self.request("PATCH", url)
    }

    /// A `DELETE` request.
    pub fn delete(&self, url: &str) -> RequestBuilder {
        self.request("DELETE", url)
    }

    /// A `HEAD` request: the response has no body.
    pub fn head(&self, url: &str) -> RequestBuilder {
        self.request("HEAD", url)
    }

    /// A request with any method, such as `OPTIONS`. `CONNECT` is not
    /// supported.
    pub fn request(&self, method: &str, url: &str) -> RequestBuilder {
        let mut error = None;
        if !is_token_str(method.as_bytes()) || method.len() > 32 {
            error = Some(Error::request(format!(
                "{method:?} is not a valid HTTP method"
            )));
        } else if method.eq_ignore_ascii_case("CONNECT") {
            error = Some(Error::request("CONNECT requests are not supported"));
        }
        let url = Url::parse(url).map_err(Error::request);
        RequestBuilder {
            client: self.clone(),
            method: method.to_owned(),
            url,
            headers: Vec::new(),
            body: None,
            timeout: None,
            error,
        }
    }

    /// How many connections are open to `url`'s origin, and how many of
    /// them are idle. For tests and metrics.
    pub fn connections(&self, url: &str) -> (usize, usize) {
        let Ok(url) = Url::parse(url) else {
            return (0, 0);
        };
        match self.shared.hosts.borrow().get(&url.origin()) {
            Some(pool) => pool.borrow().counts(),
            None => (0, 0),
        }
    }

    fn tls(&self) -> Result<TlsConnector, Error> {
        self.shared
            .tls
            .get_or_init(|| match &self.shared.config.tls {
                Some(c) => Ok(c.clone()),
                None => TlsConnector::with_system_roots()
                    .map(|c| c.alpn(&[b"http/1.1"]))
                    .map_err(|e| {
                        Error::io(
                            ErrorKind::Connect,
                            "no certificate authorities to check HTTPS servers against; install \
                             the ca-certificates package, or pass ClientBuilder::tls a connector \
                             (TlsConnector::with_webpki_roots, from giong-tls's webpki-roots feature)",
                            e,
                        )
                    }),
            })
            .clone()
    }

    fn host(&self, origin: &Origin) -> pool::Shared {
        self.shared
            .hosts
            .borrow_mut()
            .entry(origin.clone())
            .or_default()
            .clone()
    }

    /// A connection to `origin`: an idle one unless `fresh`, otherwise a
    /// new one, waiting in line while the origin has as many open as
    /// allowed.
    async fn checkout(&self, origin: &Origin, fresh: bool) -> Result<Lease, Error> {
        let config = &self.shared.config;
        let pool = self.host(origin);
        let taken = pool::acquire(
            &pool,
            config.max_connections_per_host,
            config.idle_timeout,
            fresh,
        )
        .await;
        if let pool::Taken::Reused(conn) = taken {
            return Ok(Lease {
                conn: Some(conn),
                pool,
                reused: true,
            });
        }
        let mut opening = Opening { pool: Some(&pool) };
        let tls = if origin.https {
            Some(self.tls()?)
        } else {
            None
        };
        let conn = Conn::open(origin, tls.as_ref(), config.connect_timeout).await?;
        opening.pool = None;
        drop(opening);
        Ok(Lease {
            conn: Some(conn),
            pool,
            reused: false,
        })
    }

    /// Sends one request, without following redirects. An idempotent
    /// request that finds its kept-alive connection closed under it is sent
    /// again on a new connection.
    async fn execute(
        &self,
        method: &str,
        url: &Url,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<Response, Error> {
        let config = &self.shared.config;
        let origin = url.origin();
        let what = format!("{method} {url}");
        let request = encode_request(method, url, headers, body, &config.user_agent);
        let limits = Limits {
            max_head_size: config.max_head_size,
            max_response_size: config.max_response_size,
        };
        let mut fresh = false;
        loop {
            let mut lease = self.checkout(&origin, fresh).await?;
            let reused = lease.reused;
            match lease
                .conn()
                .exchange(request.clone(), method == "HEAD", &limits, &what)
                .await
            {
                Ok(exchange) => {
                    if exchange.reusable {
                        lease.give_back(config.idle_timeout);
                    }
                    return Ok(Response::new(exchange, url.to_string()));
                }
                Err(Failure::NothingReceived(_)) if reused && is_idempotent(method) => {
                    drop(lease);
                    fresh = true;
                }
                Err(Failure::NothingReceived(e)) if reused => {
                    return Err(Error::new(
                        ErrorKind::Closed,
                        format!(
                            "{e}; the connection had been used before, so the server probably \
                             closed it as the request was sent. A {method} request is not sent \
                             again by itself, because the server may have processed it: retry \
                             if that is safe"
                        ),
                    ));
                }
                Err(failure) => return Err(failure.into_error()),
            }
        }
    }

    /// Sends a request and follows its redirects.
    async fn run(
        &self,
        mut method: String,
        mut url: Url,
        mut headers: Vec<(String, String)>,
        mut body: Option<Vec<u8>>,
    ) -> Result<Response, Error> {
        let max = self.shared.config.redirects;
        let mut hops = 0;
        loop {
            let response = self
                .execute(&method, &url, &headers, body.as_deref())
                .await?;
            let status = response.status();
            if max == 0 || !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let Some(location) = response.header("location") else {
                return Ok(response);
            };
            if hops == max {
                return Err(Error::new(
                    ErrorKind::Redirect,
                    format!(
                        "{method} {url} redirected more than {max} times (ClientBuilder::redirects); \
                         the last one pointed to {location:?}"
                    ),
                ));
            }
            let next = url.join(location).map_err(|m| {
                Error::new(
                    ErrorKind::Redirect,
                    format!("{method} {url} redirected to {location:?}, which is not valid: {m}"),
                )
            })?;
            if url.https && !next.https {
                return Err(Error::new(
                    ErrorKind::Redirect,
                    format!(
                        "{method} {url} redirected to {next}, which is not encrypted; a redirect \
                         from HTTPS to HTTP is not followed"
                    ),
                ));
            }
            // What browsers do: 303, and 301 or 302 after a POST, become a
            // GET without a body; 307 and 308 repeat the request as it was.
            if (status == 303 && method != "HEAD")
                || (matches!(status, 301 | 302) && method == "POST")
            {
                method = "GET".to_owned();
                body = None;
                headers.retain(|(n, _)| !n.eq_ignore_ascii_case("content-type"));
            }
            if next.origin() != url.origin() {
                headers
                    .retain(|(n, _)| !CREDENTIAL_HEADERS.iter().any(|c| n.eq_ignore_ascii_case(c)));
            }
            drop(response);
            url = next;
            hops += 1;
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("config", &self.shared.config)
            .field("origins", &self.shared.hosts.borrow().len())
            .finish()
    }
}

fn is_idempotent(method: &str) -> bool {
    matches!(
        method,
        "GET" | "HEAD" | "PUT" | "DELETE" | "OPTIONS" | "TRACE"
    )
}

fn encode_request(
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    user_agent: &str,
) -> Vec<u8> {
    let body_len = body.map_or(0, <[u8]>::len);
    let mut out = Vec::with_capacity(256 + url.target.len() + body_len);
    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(url.target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    if !has("host") {
        out.extend_from_slice(b"Host: ");
        out.extend_from_slice(url.host_header().as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if !has("user-agent") && !user_agent.is_empty() {
        out.extend_from_slice(b"User-Agent: ");
        out.extend_from_slice(user_agent.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    for (name, value) in headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if body.is_some() || matches!(method, "POST" | "PUT" | "PATCH") {
        out.extend_from_slice(format!("Content-Length: {body_len}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    if let Some(body) = body {
        out.extend_from_slice(body);
    }
    out
}

/// A request being put together; [`send`](RequestBuilder::send) sends it.
///
/// A mistake (a URL or header that is not valid) is kept and returned by
/// `send`, so calls can be chained.
#[must_use = "a request does nothing until it is sent"]
pub struct RequestBuilder {
    client: Client,
    method: String,
    url: Result<Url, Error>,
    headers: Vec<(String, String)>,
    body: Option<Vec<u8>>,
    timeout: Option<Duration>,
    error: Option<Error>,
}

impl RequestBuilder {
    /// Adds a header. `Host` and `User-Agent` replace the client's own;
    /// the headers that frame the message (`Content-Length`,
    /// `Transfer-Encoding`, `Connection`...) are the client's to set, and
    /// setting one is an error.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if self.error.is_some() {
            return self;
        }
        if !is_token_str(name.as_bytes()) {
            self.error = Some(Error::request(format!(
                "{name:?} is not a valid header name"
            )));
        } else if MANAGED_HEADERS.iter().any(|m| name.eq_ignore_ascii_case(m)) {
            self.error = Some(Error::request(format!(
                "the {name} header is set by the client, from the body and the connection"
            )));
        } else if !is_field_value(value.as_bytes()) || value.contains(['\r', '\n']) {
            self.error = Some(Error::request(format!(
                "the value of the {name} header has a control character (a line break?) in it"
            )));
        } else {
            self.headers
                .push((name.to_owned(), value.trim().to_owned()));
        }
        self
    }

    /// Sets the body; `Content-Length` is set from it.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// Sets a JSON body (already serialized) and `Content-Type:
    /// application/json`, unless a `Content-Type` was given.
    pub fn json(mut self, body: impl Into<Vec<u8>>) -> Self {
        if !self
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("content-type"))
        {
            self.headers
                .push(("Content-Type".to_owned(), "application/json".to_owned()));
        }
        self.body(body)
    }

    /// The timeout for this request instead of the client's
    /// ([`ClientBuilder::timeout`]).
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sends the request and reads the whole response, following
    /// redirects.
    ///
    /// Any status is a response, 404 and 500 included: see
    /// [`Response::error_for_status`]. Dropping the future closes the
    /// connection it was using.
    pub async fn send(self) -> Result<Response, Error> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let url = self.url?;
        let timeout = self.timeout.unwrap_or(self.client.shared.config.timeout);
        let what = format!("{} {url}", self.method);
        let run = self.client.run(self.method, url, self.headers, self.body);
        match giong::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "{what} did not finish within {timeout:?}; if the server is known to be this \
                     slow, raise the timeout (ClientBuilder::timeout, or RequestBuilder::timeout \
                     for one request)"
                ),
            )),
        }
    }
}

impl fmt::Debug for RequestBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestBuilder")
            .field("method", &self.method)
            .field("url", &self.url.as_ref().map(ToString::to_string))
            .field("headers", &self.headers.len())
            .field("body", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

/// A response, read whole.
pub struct Response {
    status: u16,
    /// The head's bytes, then the body.
    data: Vec<u8>,
    fields: Vec<(Range<usize>, Range<usize>)>,
    body_start: usize,
    url: String,
}

impl Response {
    fn new(exchange: conn::Exchange, url: String) -> Self {
        Self {
            status: exchange.head.status,
            fields: exchange.head.fields,
            body_start: exchange.head.len,
            data: exchange.data,
            url,
        }
    }

    /// The status code, such as 200 or 404.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The response, or an error of kind [`ErrorKind::Status`] for a 4xx or
    /// 5xx status, whose message has the start of the body (where APIs say
    /// what was wrong).
    pub fn error_for_status(self) -> Result<Self, Error> {
        if self.status < 400 {
            return Ok(self);
        }
        let body = self.body();
        let shown = String::from_utf8_lossy(&body[..body.len().min(300)]);
        let more = if body.len() > 300 { "..." } else { "" };
        Err(Error::from_status(
            self.status,
            format!(
                "{} answered {} {}: {shown:?}{more}",
                self.url,
                self.status,
                crate::response::reason(self.status)
            ),
        ))
    }

    /// The first header with this name (in any case), if its value is
    /// valid UTF-8.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| std::str::from_utf8(v).ok())
    }

    /// Every header, in the order the server sent them.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.fields.iter().map(|(n, v)| {
            // Names are tokens, so ASCII.
            let name = std::str::from_utf8(&self.data[n.clone()]).unwrap_or("");
            (name, &self.data[v.clone()])
        })
    }

    /// The body.
    pub fn body(&self) -> &[u8] {
        &self.data[self.body_start..]
    }

    /// The body as text, or an error of kind [`ErrorKind::Decode`] if it is
    /// not UTF-8.
    pub fn text(&self) -> Result<&str, Error> {
        std::str::from_utf8(self.body()).map_err(|e| {
            Error::new(
                ErrorKind::Decode,
                format!(
                    "the body of {} is not UTF-8 text ({e}); read it with Response::body",
                    self.url
                ),
            )
        })
    }

    /// The body, taken out of the response.
    pub fn into_body(mut self) -> Vec<u8> {
        self.data.split_off(self.body_start)
    }

    /// The URL that answered, after any redirects.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("url", &self.url)
            .field("headers", &self.fields.len())
            .field("body", &self.body().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_encoded_with_host_agent_and_length() {
        let url = Url::parse("http://example.com:8080/a?b=1").unwrap();
        let out = encode_request(
            "POST",
            &url,
            &[("X-Id".into(), "7".into())],
            Some(b"hi"),
            "ua/1",
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "POST /a?b=1 HTTP/1.1\r\nHost: example.com:8080\r\nUser-Agent: ua/1\r\nX-Id: 7\r\n\
             Content-Length: 2\r\n\r\nhi"
        );
        let url = Url::parse("https://example.com/").unwrap();
        let headers = [("host".to_owned(), "other".to_owned())];
        let out = encode_request("GET", &url, &headers, None, "");
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "GET / HTTP/1.1\r\nhost: other\r\n\r\n"
        );
        let out = encode_request("PUT", &url, &[], None, "");
        assert!(String::from_utf8(out)
            .unwrap()
            .ends_with("Content-Length: 0\r\n\r\n"));
    }

    #[test]
    fn bad_requests_are_refused_before_sending() {
        let client = Client::new();
        for builder in [
            client.get("example.com"),
            client.request("GE T", "http://x/"),
            client.request("CONNECT", "http://x/"),
            client.get("http://x/").header("Content-Length", "5"),
            client
                .get("http://x/")
                .header("transfer-encoding", "chunked"),
            client.get("http://x/").header("Bad Name", "1"),
            client.get("http://x/").header("X", "a\r\nInjected: yes"),
        ] {
            let e = giong::block_on(builder.send()).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Request, "{e}");
        }
    }
}
