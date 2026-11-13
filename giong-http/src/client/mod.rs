//! An HTTP client (feature `client`): HTTP/1.1, and HTTP/2 where the server
//! offers it.
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
//! * HTTP/2 when an `https` server chooses it through ALPN (see
//!   [`ClientBuilder::http2`]): one connection carries every request to the
//!   origin at once, as many as the server allows (another connection is
//!   opened, up to the limit, when it allows no more), and what a server
//!   can make the client hold is bounded by flow control (1 MiB for each
//!   response, 4 MiB for the connection, granted again only as bodies are
//!   read). A request the server refused unprocessed is sent again, and a
//!   dropped request resets its stream instead of closing the connection.
//!   [`ClientBuilder::http2_prior_knowledge`] speaks it to `http` servers
//!   known to accept it; [`Response::version`] says which was spoken.
//! * Every request has a timeout (30 s by default) that covers waiting for
//!   a connection, connecting, sending, and reading the whole response,
//!   redirects included; connecting has its own (10 s).
//! * Redirects (301, 302, 303, 307, 308) are followed, up to 10. A redirect
//!   from HTTPS to HTTP is refused, and `Authorization`, `Cookie` and a
//!   `Host` set by hand are not sent to another origin.
//! * Responses with `Content-Length`, chunked, or ending with the
//!   connection; 1xx responses are skipped. The whole body is read, up to
//!   [`max_response_size`](ClientBuilder::max_response_size) (16 MiB by
//!   default).
//! * Strict: a malformed or ambiguous response (two lengths, bare line
//!   feeds, folded headers) is an error, and the connection is not used
//!   again.
//! * HTTP proxies, when given one ([`ClientBuilder::proxy`], or
//!   [`Proxy::from_env`] for `HTTPS_PROXY`, `HTTP_PROXY` and `NO_PROXY`):
//!   `https` requests go through a `CONNECT` tunnel, so they stay encrypted
//!   end to end. The environment is not read unless asked.
//!
//! Not there yet: streaming request bodies, cookies.

mod conn;
mod decode;
mod error;
mod h2;
mod head;
mod pool;
mod proxy;
mod url;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong_tls::TlsConnector;

use self::conn::{Conn, Failure};
use self::decode::{DecodeError, Encoding, Inflate};
use self::head::Head;
use self::proxy::Route;
use self::url::Url;
use crate::parse::{is_field_value, is_token_str};
use crate::Version;

pub use self::error::{Error, ErrorKind};
pub use self::proxy::Proxy;

/// Checks for the randomised tests and the fuzz targets: they return
/// whether the input was accepted, and panic if a property does not hold.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzzing {
    use super::decode::{Encoding, Inflate};
    use super::head::{parse_head, Framing};
    use super::url::Url;
    use crate::parse::{find_head_end, is_token_str};

    /// A response head: parsed whole or refused, and what is accepted is
    /// consistent.
    pub fn check_response_head(input: &[u8]) -> bool {
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

    /// A URL, and a redirect's `Location` resolved against it.
    pub fn check_url(input: &str, location: &str) -> bool {
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

    /// A compressed body: a byte choosing gzip or deflate and how the
    /// input is cut, two bytes of output limit, then the body. Decoding
    /// never panics, never gives more than the limit at once, and either
    /// ends cleanly or says why not.
    pub fn check_inflate(data: &[u8]) -> bool {
        let [how, a, b, input @ ..] = data else {
            return false;
        };
        let encoding = if how & 1 == 0 {
            Encoding::Gzip
        } else {
            Encoding::Deflate
        };
        let step = 1 + usize::from(how >> 1) * 37;
        let limit = 1 + usize::from(u16::from_le_bytes([*a, *b]));
        let mut d = Inflate::new(encoding);
        let mut total = 0usize;
        for piece in input.chunks(step) {
            d.push(piece);
            loop {
                let mut out = Vec::new();
                if d.pull(&mut out, limit).is_err() {
                    return false;
                }
                assert!(out.len() <= limit);
                total += out.len();
                // A body that expands without end is stopped by the
                // caller's limit; so is this check.
                if out.is_empty() || total > 16 << 20 {
                    break;
                }
            }
            if total > 16 << 20 {
                return false;
            }
        }
        d.finish().is_ok()
    }

    /// Frames an HTTP/2 server might send, built from `data`, fed in
    /// pieces to a client connection while requests come and go: what the
    /// client holds stays within the windows it grants, and what it writes
    /// is well formed and within the server's windows. Returns the number
    /// of responses read.
    pub fn check_h2_client(data: &[u8]) -> usize {
        super::h2::fuzzing::check_server_frames(data)
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
    "proxy-authorization",
];

/// Headers that carry credentials, which are not sent to another origin
/// after a redirect.
const CREDENTIAL_HEADERS: &[&str] = &["authorization", "cookie", "proxy-authorization"];

/// How often, at most, the idle connections of every origin are looked at,
/// so that those of an origin no longer asked for are closed too.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

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
    proxy: Option<Proxy>,
    decompress: bool,
    http2: bool,
    http2_prior_knowledge: bool,
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
            proxy: None,
            decompress: true,
            http2: true,
            http2_prior_knowledge: false,
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
        self.tls = Some(connector);
        self
    }

    /// Whether to offer HTTP/2 to `https` servers through ALPN (default
    /// `true`). A server that chooses it gets every request to its origin on
    /// one connection, as many at once as it allows; one that does not is
    /// spoken to over HTTP/1.1. The connector given to [`tls`](Self::tls)
    /// gets its ALPN protocols from this.
    pub fn http2(mut self, on: bool) -> Self {
        self.http2 = on;
        self
    }

    /// Speaks HTTP/2 to `http` URLs from the first byte, without asking
    /// ("prior knowledge", default `false`): for servers known to accept it,
    /// such as gRPC backends and giong-http's own server. Not through a
    /// proxy, where `http` URLs stay on HTTP/1.1.
    pub fn http2_prior_knowledge(mut self, on: bool) -> Self {
        self.http2_prior_knowledge = on;
        self
    }

    /// Whether to ask for compressed responses (`Accept-Encoding: gzip,
    /// deflate`) and decode them (default `true`). The limits apply to the
    /// decoded body, so a small response that would expand past
    /// [`max_response_size`](Self::max_response_size) fails rather than
    /// filling memory. A request that sets its own `Accept-Encoding` gets
    /// its body as the server sent it, and so does a response in an
    /// encoding the client does not decode (`br`, `zstd`).
    pub fn decompress(mut self, on: bool) -> Self {
        self.decompress = on;
        self
    }

    /// Sends requests through an HTTP proxy (none by default; the
    /// environment is read only through [`Proxy::from_env`]). Called again,
    /// it adds to what was given: [`Proxy::http`] then [`Proxy::https`] sets
    /// one proxy for each scheme.
    pub fn proxy(mut self, proxy: Proxy) -> Self {
        match &mut self.proxy {
            Some(p) => p.merge(proxy),
            None => self.proxy = Some(proxy),
        }
        self
    }

    /// The client.
    pub fn build(self) -> Client {
        Client {
            shared: Rc::new(Shared {
                tls: OnceCell::new(),
                config: self,
                hosts: RefCell::new(HashMap::new()),
                swept: Cell::new(Instant::now()),
            }),
        }
    }
}

/// An HTTP client with a pool of connections for each origin.
///
/// Cheap to clone; clones share the pool. Not `Send`: make one per worker.
#[derive(Clone)]
pub struct Client {
    shared: Rc<Shared>,
}

struct Shared {
    config: ClientBuilder,
    tls: OnceCell<Result<TlsConnector, Error>>,
    hosts: RefCell<HashMap<Route, pool::Shared>>,
    /// When the idle connections of every route were last looked at.
    swept: Cell<Instant>,
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
        if let Some(mut conn) = self.conn.take() {
            conn.idle_since = std::time::Instant::now();
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

/// What a request got to be sent on.
enum Checkout {
    /// An HTTP/1.1 connection of its own.
    H1(Lease),
    /// An HTTP/2 connection just opened.
    H2(h2::Connection),
    /// An HTTP/2 connection that was there already.
    Shared(h2::Connection),
    /// Room may have appeared on an HTTP/2 connection: look again.
    Retry,
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
        match self.shared.hosts.borrow().get(&self.route(&url)) {
            Some(pool) => pool.borrow().counts(),
            None => (0, 0),
        }
    }

    fn tls(&self) -> Result<TlsConnector, Error> {
        let alpn: &[&[u8]] = if self.shared.config.http2 {
            &[b"h2", b"http/1.1"]
        } else {
            &[b"http/1.1"]
        };
        self.shared
            .tls
            .get_or_init(|| match &self.shared.config.tls {
                Some(c) => Ok(c.clone().alpn(alpn)),
                None => TlsConnector::with_system_roots()
                    .map(|c| c.alpn(alpn))
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

    /// Where connections for `url` go: through the proxy, or not.
    fn route(&self, url: &Url) -> Route {
        match &self.shared.config.proxy {
            Some(proxy) => proxy.route(url),
            None => Route {
                origin: url.origin(),
                proxy: None,
            },
        }
    }

    fn host(&self, route: &Route) -> pool::Shared {
        self.sweep();
        let mut hosts = self.shared.hosts.borrow_mut();
        // Looked up first so that the route is only copied the first time.
        if let Some(pool) = hosts.get(route) {
            return pool.clone();
        }
        hosts.entry(route.clone()).or_default().clone()
    }

    /// Closes the connections of every route (an origin, directly or
    /// through a proxy) that have been idle for the idle timeout, and
    /// forgets the routes left with none. A route's own requests close its
    /// expired connections as they go; this catches those of routes that are
    /// not asked for again, which would otherwise stay open, along with their
    /// entries, for as long as the client lives.
    fn sweep(&self) {
        let shared = &self.shared;
        let idle_timeout = shared.config.idle_timeout;
        if shared.swept.get().elapsed() < SWEEP_EVERY.min(idle_timeout) {
            return;
        }
        shared.swept.set(Instant::now());
        // A route some request still holds is kept, even when empty.
        shared
            .hosts
            .borrow_mut()
            .retain(|_, pool| !pool::sweep(pool, idle_timeout) || Rc::strong_count(pool) > 1);
    }

    /// A connection on `route`: an idle one unless `fresh`, otherwise a
    /// new one, waiting in line while the origin has as many open as
    /// allowed. A new connection that speaks HTTP/2 joins the pool, shared.
    async fn checkout(&self, route: &Route, fresh: bool) -> Result<Checkout, Error> {
        let config = &self.shared.config;
        let pool = self.host(route);
        let prior_knowledge =
            config.http2_prior_knowledge && !route.origin.https && route.proxy.is_none();
        let taken = pool::acquire(
            &pool,
            config.max_connections_per_host,
            config.idle_timeout,
            fresh,
            prior_knowledge,
        )
        .await;
        match taken {
            pool::Taken::Reused(conn) => {
                return Ok(Checkout::H1(Lease {
                    conn: Some(conn),
                    pool,
                    reused: true,
                }))
            }
            pool::Taken::Retry => return Ok(Checkout::Retry),
            pool::Taken::Open => {}
        }
        let mut opening = Opening { pool: Some(&pool) };
        let tls = if route.origin.https {
            Some(self.tls()?)
        } else {
            None
        };
        // Boxed: opening is rare next to reusing a connection, and its
        // future (DNS, TCP, a proxy tunnel, the TLS handshake) is large, so
        // it would otherwise make every request's future larger to move.
        let conn = Box::pin(Conn::open(route, tls.as_ref(), config)).await?;
        opening.pool = None;
        drop(opening);
        if conn.is_h2() || prior_knowledge {
            return Ok(Checkout::H2(self.start_h2(&pool, conn)));
        }
        if route.origin.https {
            pool::opened_h1(&pool);
        }
        Ok(Checkout::H1(Lease {
            conn: Some(conn),
            pool,
            reused: false,
        }))
    }

    /// Speaks HTTP/2 on a connection just opened, and puts it in the pool.
    fn start_h2(&self, pool: &pool::Shared, conn: Conn) -> h2::Connection {
        let config = &self.shared.config;
        let id = pool::next_h2_id(pool);
        let weak = Rc::downgrade(pool);
        let notify = Box::new(move |event| {
            if let Some(pool) = weak.upgrade() {
                match event {
                    h2::Event::StreamClosed => pool::h2_stream_closed(&pool),
                    h2::Event::Closed => pool::h2_closed(&pool, id),
                }
            }
        });
        let settings = h2::Settings {
            max_head_size: config.max_head_size,
            idle_timeout: config.idle_timeout,
            write_timeout: config.timeout,
        };
        let h2 = h2::Connection::start(conn.into_stream(), settings, notify);
        pool::add_h2(pool, id, h2.clone());
        h2
    }

    /// Sends one request and reads the head of its response, without
    /// following redirects. An idempotent request that finds its
    /// kept-alive connection closed under it is sent again on a new
    /// connection; so is any request an HTTP/2 server refused unprocessed.
    async fn execute(
        &self,
        method: &str,
        url: &Url,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<Pending, Error> {
        let config = &self.shared.config;
        let route = self.route(url);
        let what = format!("{method} {url}");
        let decompress = config.decompress
            && !headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("accept-encoding"));
        let mut request = None;
        let mut fresh = false;
        let mut refused = 0;
        loop {
            // An HTTP/2 connection with room takes the request at once.
            let pool = self.host(&route);
            let checkout = match pool::h2_with_room(&pool) {
                Some(conn) => Checkout::Shared(conn),
                None => self.checkout(&route, fresh).await?,
            };
            drop(pool);
            let (conn, reused) = match checkout {
                Checkout::Retry => continue,
                Checkout::Shared(conn) => (conn, true),
                Checkout::H2(conn) => (conn, false),
                Checkout::H1(mut lease) => {
                    let request = request.get_or_insert_with(|| {
                        encode_request(
                            method,
                            url,
                            headers,
                            body,
                            &config.user_agent,
                            route.absolute_form(),
                            decompress,
                        )
                    });
                    let reused = lease.reused;
                    match lease
                        .conn()
                        .send(
                            request.clone(),
                            method == "HEAD",
                            config.max_head_size,
                            &what,
                        )
                        .await
                    {
                        Ok(answer) => {
                            return Ok(Pending::new(
                                Some(lease),
                                Body::H1(answer.body),
                                answer.head,
                                url,
                                what,
                                decompress,
                            ))
                        }
                        Err(Failure::NothingReceived(_)) if reused && is_idempotent(method) => {
                            drop(lease);
                            fresh = true;
                            continue;
                        }
                        Err(Failure::NothingReceived(e)) if reused => {
                            return Err(not_sent_again(e, method))
                        }
                        Err(failure) => return Err(failure.into_error()),
                    }
                }
            };
            let fields = h2_fields(method, headers, body, &config.user_agent, decompress);
            let authority = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("host"))
                .map_or_else(|| url.host_header(), |(_, v)| v.clone());
            let req = h2::Request {
                method,
                scheme: if url.https { "https" } else { "http" },
                authority: &authority,
                path: &url.target,
                fields: &fields,
                body,
                head_request: method == "HEAD",
            };
            let Some(mut stream) = conn.send(&req) else {
                // Its room went to another request meanwhile.
                continue;
            };
            match stream.response().await {
                Ok(()) => {
                    let head = h2_head(stream.head());
                    return Ok(Pending::new(
                        None,
                        Body::H2(stream),
                        head,
                        url,
                        what,
                        decompress,
                    ));
                }
                Err(h2::StreamError::Refused(m)) => {
                    refused += 1;
                    if refused > 5 {
                        return Err(Error::new(
                            ErrorKind::Closed,
                            format!("{what}: {m}, five times in a row"),
                        ));
                    }
                }
                Err(h2::StreamError::Closed {
                    received: false,
                    message,
                }) if reused => {
                    let e = Error::new(ErrorKind::Closed, format!("{what}: {message}"));
                    if !is_idempotent(method) {
                        return Err(not_sent_again(e, method));
                    }
                    fresh = true;
                }
                Err(e) => return Err(h2_error(e, &what)),
            }
        }
    }

    /// Sends a request and follows its redirects, up to the head of the
    /// last response.
    async fn run(
        &self,
        mut method: String,
        mut url: Url,
        mut headers: Vec<(String, String)>,
        mut body: Option<Vec<u8>>,
    ) -> Result<Pending, Error> {
        let config = &self.shared.config;
        let max = config.redirects;
        let mut hops = 0;
        loop {
            let pending = self
                .execute(&method, &url, &headers, body.as_deref())
                .await?;
            let status = pending.head.status;
            if max == 0 || !matches!(status, 301 | 302 | 303 | 307 | 308) {
                return Ok(pending);
            }
            let Some(location) = pending.header("location").map(str::to_owned) else {
                return Ok(pending);
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
            let next = url.join(&location).map_err(|m| {
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
                // A Host set for the first origin would name the wrong one
                // to the next: it gets its own.
                headers.retain(|(n, _)| {
                    !n.eq_ignore_ascii_case("host")
                        && !CREDENTIAL_HEADERS.iter().any(|c| n.eq_ignore_ascii_case(c))
                });
            }
            // The redirect's own body is read and dropped, so that its
            // connection can serve the next request.
            pending.read_whole(config).await?;
            url = next;
            hops += 1;
        }
    }
}

/// A response whose head has arrived, with the connection its body comes
/// on.
struct Pending {
    /// The HTTP/1.1 connection; none over HTTP/2, where the stream is in
    /// `body`.
    lease: Option<Lease>,
    head: Head,
    /// Holds the head's bytes too.
    body: Body,
    /// Set when the body is compressed and the client asked for that.
    inflate: Option<Box<Inflate>>,
    url: String,
    /// `METHOD url`, for messages.
    what: String,
}

/// The body of a response, over either protocol.
enum Body {
    H1(conn::Body),
    H2(h2::Stream),
}

impl Body {
    fn head_bytes(&self) -> &[u8] {
        match self {
            Body::H1(b) => b.head_bytes(),
            Body::H2(s) => &s.head().bytes,
        }
    }

    /// The length of the body, when known.
    fn length(&self) -> Option<u64> {
        match self {
            Body::H1(b) => b.length(),
            Body::H2(s) => s.body_length(),
        }
    }

    /// The buffer holding the head and what came with it, when that is
    /// the whole body.
    fn take_with_head(&mut self) -> Option<Vec<u8>> {
        match self {
            Body::H1(b) => b.take_with_head(),
            Body::H2(_) => None,
        }
    }

    /// Appends the next part of the body to `out`, about `limit` bytes at
    /// most; `false` at the end.
    async fn read(
        &mut self,
        conn: Option<&Conn>,
        out: &mut Vec<u8>,
        limit: usize,
        what: &str,
    ) -> Result<bool, Error> {
        match self {
            Body::H1(b) => {
                let conn = conn.expect("an HTTP/1.1 body comes with its connection");
                b.read(conn, out, what).await
            }
            Body::H2(s) => s.read(out, limit).await.map_err(|e| h2_error(e, what)),
        }
    }

    /// The body has been read to its end.
    fn is_done(&self) -> bool {
        match self {
            Body::H1(b) => b.is_done(),
            Body::H2(s) => s.is_done(),
        }
    }

    /// The HTTP/1.1 connection can carry another request.
    fn reusable(&self) -> bool {
        match self {
            Body::H1(b) => b.reusable(),
            Body::H2(_) => false,
        }
    }
}

impl Pending {
    fn new(
        lease: Option<Lease>,
        body: Body,
        mut head: Head,
        url: &Url,
        what: String,
        decompress: bool,
    ) -> Self {
        let head_bytes = body.head_bytes();
        let mut inflate = None;
        if decompress && head.status != 206 && body.length() != Some(0) {
            let encoding = header_in(&head, head_bytes, "content-encoding").map(Encoding::parse);
            if let Some(Ok(Some(encoding))) = encoding {
                inflate = Some(Box::new(Inflate::new(encoding)));
                // The body given out is the decoded one: these describe
                // the encoded one.
                head.fields.retain(|(n, _)| {
                    let name = &head_bytes[n.clone()];
                    !name.eq_ignore_ascii_case(b"content-encoding")
                        && !name.eq_ignore_ascii_case(b"content-length")
                });
            }
        }
        Self {
            lease,
            head,
            body,
            inflate,
            url: url.to_string(),
            what,
        }
    }

    fn version(&self) -> Version {
        match (&self.body, self.head.minor) {
            (Body::H2(_), _) => Version::Http2,
            (Body::H1(_), 0) => Version::Http10,
            (Body::H1(_), _) => Version::Http11,
        }
    }

    /// The HTTP/1.1 connection, if the body comes on one.
    fn conn(&self) -> Option<&Conn> {
        self.lease.as_ref().and_then(|l| l.conn.as_ref())
    }

    /// The body has ended or failed: nothing more comes.
    fn finished(&self) -> bool {
        match &self.body {
            Body::H1(_) => self.conn().is_none(),
            Body::H2(s) => s.is_finished(),
        }
    }

    /// Stops reading: closes the HTTP/1.1 connection, or resets the stream.
    fn close(&mut self) {
        match &mut self.body {
            Body::H1(_) => {
                if let Some(lease) = &mut self.lease {
                    close(lease);
                }
            }
            Body::H2(s) => s.cancel(),
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        header_in(&self.head, self.body.head_bytes(), name)
            .and_then(|v| std::str::from_utf8(v).ok())
    }

    /// Reads the rest of the body, decoded, up to the client's limit.
    async fn read_whole(mut self, config: &ClientBuilder) -> Result<Response, Error> {
        let max = config.max_response_size;
        let what = &self.what;
        let too_large = |decoded: bool| {
            Error::new(
                ErrorKind::TooLarge,
                format!(
                    "{what}: the response body is larger than {max} bytes{} \
                     (ClientBuilder::max_response_size; RequestBuilder::send_streaming reads a \
                     body of any size piece by piece)",
                    if decoded { " once decompressed" } else { "" }
                ),
            )
        };
        if self.body.length().is_some_and(|n| n > max as u64) {
            return Err(too_large(false));
        }
        let start = self.body.head_bytes().len();
        let mut data = match self.inflate {
            None => self.body.take_with_head(),
            Some(_) => None,
        }
        .unwrap_or_else(|| self.body.head_bytes().to_vec());
        if let Some(n) = self.body.length() {
            data.reserve(n as usize);
        }
        let conn = self.lease.as_ref().and_then(|l| l.conn.as_ref());
        match &mut self.inflate {
            None => {
                while self.body.read(conn, &mut data, usize::MAX, what).await? {
                    if data.len() - start > max {
                        return Err(too_large(false));
                    }
                }
            }
            Some(inflate) => {
                let mut raw = Vec::new();
                let mut received = 0;
                loop {
                    raw.clear();
                    let more = self.body.read(conn, &mut raw, usize::MAX, what).await?;
                    received += raw.len();
                    if received > max {
                        return Err(too_large(false));
                    }
                    inflate.push(&raw);
                    inflate
                        .pull(&mut data, start + max + 1)
                        .map_err(|e| decode_error(what, e))?;
                    if data.len() - start > max {
                        return Err(too_large(true));
                    }
                    if !more {
                        inflate.finish().map_err(|e| decode_error(what, e))?;
                        break;
                    }
                }
            }
        }
        if self.body.reusable() {
            if let Some(lease) = self.lease.take() {
                lease.give_back(config.idle_timeout);
            }
        }
        Ok(Response {
            status: self.head.status,
            version: self.version(),
            fields: self.head.fields,
            body_start: start,
            data,
            url: self.url,
        })
    }
}

/// The value of the first header named `name` in a head.
fn header_in<'a>(head: &Head, bytes: &'a [u8], name: &str) -> Option<&'a [u8]> {
    head.fields
        .iter()
        .find(|(n, _)| bytes[n.clone()].eq_ignore_ascii_case(name.as_bytes()))
        .map(|(_, v)| &bytes[v.clone()])
}

fn decode_error(what: &str, e: DecodeError) -> Error {
    let DecodeError::Damaged(m) = e;
    Error::new(
        ErrorKind::Decode,
        format!(
            "{what}: the body cannot be decompressed: {m}. To read it as it came, send the \
             request with its own Accept-Encoding header, or turn decompression off with \
             ClientBuilder::decompress(false)"
        ),
    )
}

/// A request that may have been processed is not sent again.
fn not_sent_again(e: Error, method: &str) -> Error {
    Error::new(
        ErrorKind::Closed,
        format!(
            "{e}; the connection had been used before, so the server probably closed it as \
             the request was sent. A {method} request is not sent again by itself, because the \
             server may have processed it: retry if that is safe"
        ),
    )
}

/// The fields of an HTTP/2 request after its pseudo-headers: the same as
/// over HTTP/1.1, in lowercase, with `Host` turned into `:authority`.
fn h2_fields(
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    user_agent: &str,
    accept_encoding: bool,
) -> Vec<(String, String)> {
    let mut fields = Vec::with_capacity(headers.len() + 3);
    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    if !has("user-agent") && !user_agent.is_empty() {
        fields.push(("user-agent".to_owned(), user_agent.to_owned()));
    }
    if accept_encoding {
        fields.push((
            "accept-encoding".to_owned(),
            decode::ACCEPT_ENCODING.to_owned(),
        ));
    }
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            continue;
        }
        fields.push((name.to_ascii_lowercase(), value.clone()));
    }
    if body.is_some() || matches!(method, "POST" | "PUT" | "PATCH") {
        fields.push((
            "content-length".to_owned(),
            body.map_or(0, <[u8]>::len).to_string(),
        ));
    }
    fields
}

/// The head of an HTTP/2 response, as the rest of the client reads heads.
fn h2_head(head: &h2::ResponseHead) -> Head {
    Head {
        status: head.status,
        minor: 1,
        fields: head.fields.clone(),
        len: head.bytes.len(),
        content_length: head.content_length,
        chunked: false,
        close: false,
    }
}

fn h2_error(e: h2::StreamError, what: &str) -> Error {
    match e {
        h2::StreamError::Refused(m)
        | h2::StreamError::Reset(m)
        | h2::StreamError::Closed { message: m, .. } => {
            Error::new(ErrorKind::Closed, format!("{what}: {m}"))
        }
        h2::StreamError::Protocol(m) => Error::new(
            ErrorKind::Protocol,
            format!("{what}: the server sent an invalid HTTP/2 response: {m}"),
        ),
        h2::StreamError::TooLarge(m) => Error::new(ErrorKind::TooLarge, format!("{what}: {m}")),
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
    proxy: Option<&proxy::Server>,
    accept_encoding: bool,
) -> Vec<u8> {
    let body_len = body.map_or(0, <[u8]>::len);
    let mut out = Vec::with_capacity(256 + url.target.len() + body_len);
    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    if proxy.is_some() {
        // Absolute form, for a proxy (RFC 9112 section 3.2.2).
        out.extend_from_slice(b"http://");
        out.extend_from_slice(url.host_header().as_bytes());
    }
    out.extend_from_slice(url.target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    if let Some(auth) = proxy.and_then(|p| p.authorization.as_deref()) {
        out.extend_from_slice(b"Proxy-Authorization: ");
        out.extend_from_slice(auth.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
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
    if accept_encoding {
        out.extend_from_slice(b"Accept-Encoding: ");
        out.extend_from_slice(decode::ACCEPT_ENCODING.as_bytes());
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
    /// redirects. A compressed body is decompressed (see
    /// [`ClientBuilder::decompress`]).
    ///
    /// Any status is a response, 404 and 500 included: see
    /// [`Response::error_for_status`]. Dropping the future closes the
    /// connection it was using.
    pub async fn send(self) -> Result<Response, Error> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let url = self.url?;
        let client = self.client;
        let timeout = self.timeout.unwrap_or(client.shared.config.timeout);
        let what = format!("{} {url}", self.method);
        let run = async {
            let pending = client
                .run(self.method, url, self.headers, self.body)
                .await?;
            pending.read_whole(&client.shared.config).await
        };
        match giong::time::timeout(timeout, run).await {
            Ok(result) => result,
            Err(_) => Err(timed_out(&what, timeout)),
        }
    }

    /// Sends the request and returns once the head of the response has
    /// arrived, following redirects; the body is then read piece by piece
    /// with [`StreamingResponse::chunk`], with no limit on its size. For
    /// downloads and long responses that should not be held whole.
    ///
    /// The request's timeout covers everything up to the head. After that,
    /// it is the longest wait for each piece of the body, so a body that
    /// keeps arriving can take as long as it needs, and one that stops is
    /// noticed.
    pub async fn send_streaming(self) -> Result<StreamingResponse, Error> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let url = self.url?;
        let client = self.client;
        let timeout = self.timeout.unwrap_or(client.shared.config.timeout);
        let what = format!("{} {url}", self.method);
        let run = client.run(self.method, url, self.headers, self.body);
        match giong::time::timeout(timeout, run).await {
            Ok(Ok(pending)) => Ok(StreamingResponse {
                pending,
                timeout,
                idle_timeout: client.shared.config.idle_timeout,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(timed_out(&what, timeout)),
        }
    }
}

fn timed_out(what: &str, timeout: Duration) -> Error {
    Error::new(
        ErrorKind::Timeout,
        format!(
            "{what} did not finish within {timeout:?}; if the server is known to be this \
             slow, raise the timeout (ClientBuilder::timeout, or RequestBuilder::timeout \
             for one request)"
        ),
    )
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
    version: Version,
    /// The head's bytes, then the body.
    data: Vec<u8>,
    fields: Vec<(Range<usize>, Range<usize>)>,
    body_start: usize,
    url: String,
}

impl Response {
    /// The status code, such as 200 or 404.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The version of HTTP the response came in: [`Version::Http2`] when
    /// the server chose it (through ALPN, or with
    /// [`ClientBuilder::http2_prior_knowledge`]).
    pub fn version(&self) -> Version {
        self.version
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
            .field("version", &self.version)
            .field("url", &self.url)
            .field("headers", &self.fields.len())
            .field("body", &self.body().len())
            .finish()
    }
}

/// A response whose body is read piece by piece, from
/// [`RequestBuilder::send_streaming`].
///
/// ```no_run
/// use giong_http::client::Client;
///
/// # fn main() -> Result<(), giong_http::client::Error> {
/// # giong::block_on(async {
/// let client = Client::new();
/// let mut response = client
///     .get("https://example.com/big.csv")
///     .send_streaming()
///     .await?
///     .error_for_status()?;
/// let mut total = 0;
/// while let Some(piece) = response.chunk().await? {
///     total += piece.len(); // write it to a file, parse it...
/// }
/// println!("{total} bytes");
/// # Ok(())
/// # })
/// # }
/// ```
///
/// An HTTP/1.1 connection goes back to the pool once the body has been
/// read to its end. Dropped before that, the response closes its
/// connection; over HTTP/2 it resets its stream, and the connection goes
/// on carrying the others.
pub struct StreamingResponse {
    /// Its lease holds no connection once the body has ended or failed.
    pending: Pending,
    timeout: Duration,
    idle_timeout: Duration,
}

/// The most decoded bytes one [`StreamingResponse::chunk`] gives.
const MAX_CHUNK: usize = 64 * 1024;

impl StreamingResponse {
    fn pending(&self) -> &Pending {
        &self.pending
    }

    /// The status code, such as 200 or 404.
    pub fn status(&self) -> u16 {
        self.pending().head.status
    }

    /// The version of HTTP the response comes in.
    pub fn version(&self) -> Version {
        self.pending().version()
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status())
    }

    /// The response, or an error of kind [`ErrorKind::Status`] for a 4xx or
    /// 5xx status. The body is not read for the message (it may be large);
    /// the connection is closed.
    pub fn error_for_status(self) -> Result<Self, Error> {
        let status = self.status();
        if status < 400 {
            return Ok(self);
        }
        Err(Error::from_status(
            status,
            format!(
                "{} answered {status} {}",
                self.pending().url,
                crate::response::reason(status)
            ),
        ))
    }

    /// The first header with this name (in any case), if its value is
    /// valid UTF-8. When the body is decompressed, its `Content-Encoding`
    /// and `Content-Length` are left out: they describe the compressed
    /// body.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.pending().header(name)
    }

    /// Every header, in the order the server sent them.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &[u8])> {
        let p = self.pending();
        let bytes = p.body.head_bytes();
        p.head.fields.iter().map(move |(n, v)| {
            let name = std::str::from_utf8(&bytes[n.clone()]).unwrap_or("");
            (name, &bytes[v.clone()])
        })
    }

    /// The URL that answered, after any redirects.
    pub fn url(&self) -> &str {
        &self.pending().url
    }

    /// The next piece of the body (decompressed if it was compressed, at
    /// most 64 KiB), or `None` at its end. Waits at most the request's
    /// timeout for it. After an error, the connection is closed and every
    /// further call returns `None`.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, Error> {
        let pending = &mut self.pending;
        if pending.finished() {
            return Ok(None);
        }
        let timeout = self.timeout;
        let result = match giong::time::timeout(timeout, next_piece(pending)).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "{}: no part of the body arrived within {timeout:?}; if the server is known \
                     to pause this long, raise the timeout (RequestBuilder::timeout)",
                    pending.what
                ),
            )),
        };
        match &result {
            Ok(Some(_)) => {}
            Ok(None) => {
                // The end: the connection can serve another request.
                if pending.body.reusable() {
                    if let Some(lease) = &mut pending.lease {
                        if let Some(conn) = lease.conn.take() {
                            let lease = Lease {
                                conn: Some(conn),
                                pool: lease.pool.clone(),
                                reused: true,
                            };
                            lease.give_back(self.idle_timeout);
                        }
                    }
                } else {
                    pending.close();
                }
            }
            Err(_) => pending.close(),
        }
        result
    }
}

/// Closes a lease's connection now, giving its place in the pool to the
/// next request.
fn close(lease: &mut Lease) {
    if let Some(conn) = lease.conn.take() {
        drop(conn);
        pool::free(&lease.pool);
    }
}

async fn next_piece(p: &mut Pending) -> Result<Option<Vec<u8>>, Error> {
    let conn = p.lease.as_ref().and_then(|l| l.conn.as_ref());
    let what = &p.what;
    match &mut p.inflate {
        None => {
            let mut out = Vec::new();
            if p.body.read(conn, &mut out, MAX_CHUNK, what).await? {
                Ok(Some(out))
            } else {
                Ok(None)
            }
        }
        Some(inflate) => {
            let mut raw = Vec::new();
            loop {
                let mut out = Vec::new();
                inflate
                    .pull(&mut out, MAX_CHUNK)
                    .map_err(|e| decode_error(what, e))?;
                if !out.is_empty() {
                    return Ok(Some(out));
                }
                if p.body.is_done() {
                    inflate.finish().map_err(|e| decode_error(what, e))?;
                    return Ok(None);
                }
                raw.clear();
                p.body.read(conn, &mut raw, MAX_CHUNK, what).await?;
                inflate.push(&raw);
            }
        }
    }
}

impl fmt::Debug for StreamingResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = self.pending();
        f.debug_struct("StreamingResponse")
            .field("status", &p.head.status)
            .field("url", &p.url)
            .field("decompressing", &p.inflate.is_some())
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
            None,
            true,
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "POST /a?b=1 HTTP/1.1\r\nHost: example.com:8080\r\nUser-Agent: ua/1\r\n\
             Accept-Encoding: gzip, deflate\r\nX-Id: 7\r\n\
             Content-Length: 2\r\n\r\nhi"
        );
        let url = Url::parse("https://example.com/").unwrap();
        let headers = [("host".to_owned(), "other".to_owned())];
        let out = encode_request("GET", &url, &headers, None, "", None, false);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "GET / HTTP/1.1\r\nhost: other\r\n\r\n"
        );
        let out = encode_request("PUT", &url, &[], None, "", None, false);
        assert!(String::from_utf8(out)
            .unwrap()
            .ends_with("Content-Length: 0\r\n\r\n"));

        // Through a proxy: the whole URL, and the proxy's credentials.
        let proxy = Proxy::all("http://u:p@proxy:3128").unwrap();
        let url = Url::parse("http://example.com:8080/a?b=1").unwrap();
        let route = proxy.route(&url);
        let out = encode_request("GET", &url, &[], None, "", route.absolute_form(), false);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "GET http://example.com:8080/a?b=1 HTTP/1.1\r\nProxy-Authorization: Basic dTpw\r\n\
             Host: example.com:8080\r\n\r\n"
        );
        // Through a tunnel, the request is the one the server would get.
        let url = Url::parse("https://example.com/").unwrap();
        assert!(proxy.route(&url).proxy.is_some());
        assert!(proxy.route(&url).absolute_form().is_none());
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
            client
                .get("http://x/")
                .header("Proxy-Authorization", "Basic eDp5"),
            client.get("http://x/").header("X", "a\r\nInjected: yes"),
        ] {
            let e = giong::block_on(builder.send()).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Request, "{e}");
        }
    }
}
