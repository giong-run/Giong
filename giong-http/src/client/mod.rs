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
//! * HTTP/3 with the `unstable-http3` feature, to `https` servers that
//!   advertise it with `Alt-Svc` (`ClientBuilder::http3`) or are known to
//!   speak it (`ClientBuilder::http3_prior_knowledge`): one QUIC
//!   connection for each origin, a stream for each request, so a slow
//!   response holds back no other. A request the server rejected
//!   unprocessed is sent again, and a server that says it is going away
//!   (GOAWAY) gets its next requests on a new connection.
//! * [`Client::preconnect`] opens a connection before the first request
//!   needs it, and [`Client::ping_idle_http2`] checks idle HTTP/2
//!   connections with a PING, closing those that do not answer in time.
//! * Request bodies written piece by piece
//!   ([`RequestBuilder::streamed_body`], [`RequestBody`]), with trailer
//!   fields after them; over HTTP/2 the response is read while the request
//!   is still being written (full duplex). The trailer fields of a response
//!   are read with [`StreamingResponse::trailers`]; an HTTP/2 or HTTP/3
//!   request sent with [`RequestBuilder::send_streaming`] says
//!   `te: trailers`. A streamed body goes over HTTP/1.1 or HTTP/2.
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

#[cfg(feature = "unstable-http3")]
mod alt_svc;
mod conn;
mod decode;
mod error;
mod h2;
#[cfg(feature = "unstable-http3")]
mod h3;
#[cfg(all(any(test, fuzzing), feature = "unstable-http3"))]
pub(crate) mod h3_hostile;
#[cfg(all(test, feature = "unstable-http3"))]
mod h3_tests;
mod head;
mod pool;
mod proxy;
mod url;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::fmt;
#[cfg(feature = "unstable-http3")]
use std::io;
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong_tls::TlsConnector;

use self::conn::{Conn, Failure};
use self::decode::{DecodeError, Encoding, Inflate};
use self::head::Head;
use self::proxy::Route;
use self::url::Url;
use crate::body::{BodySender, StreamBody};
use crate::parse::{is_field_value, is_token_str};
use crate::trailers::Trailers;
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

    /// An `Alt-Svc` header field: read or ignored, and what is read is a
    /// usable port for a bounded time.
    #[cfg(feature = "unstable-http3")]
    pub fn check_alt_svc(input: &[u8]) -> bool {
        match super::alt_svc::parse(input) {
            super::alt_svc::AltSvc::H3 { port, max_age } => {
                assert!(port != 0);
                assert!(!max_age.is_zero() && max_age <= std::time::Duration::from_secs(604_800));
                true
            }
            super::alt_svc::AltSvc::Clear | super::alt_svc::AltSvc::NoH3 => true,
            super::alt_svc::AltSvc::Malformed => false,
        }
    }

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

    /// An HTTP/1.1 response, fed in pieces to the code that reads one on a
    /// connection: the body is exactly what its framing delimits (length,
    /// chunked, or up to the close), given out in non-empty pieces or
    /// failing as it should, and the connection is reusable only when the
    /// bytes received end exactly where the body does. Returns `true` if
    /// the whole body was read.
    pub fn check_response_body(data: &[u8]) -> bool {
        super::conn::fuzzing::check_response_body(data)
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
    #[cfg(feature = "unstable-http3")]
    http3: Option<Http3>,
}

/// The QUIC settings HTTP/3 is spoken with, and to whom.
#[cfg(feature = "unstable-http3")]
#[derive(Clone)]
struct Http3 {
    quic: giong_quic::ClientConfig,
    /// To every `https` origin, without waiting for it to say it speaks
    /// HTTP/3.
    prior_knowledge: bool,
}

/// How long connecting to an alternative an origin advertised may take
/// before the request goes over TCP instead.
#[cfg(feature = "unstable-http3")]
const ALTERNATIVE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long an alternative that could not be reached is left alone.
#[cfg(feature = "unstable-http3")]
const ALTERNATIVE_BROKEN_FOR: Duration = Duration::from_secs(5 * 60);

/// HTTP/3 as an origin advertised it with `Alt-Svc`.
#[cfg(feature = "unstable-http3")]
struct Alternative {
    port: u16,
    until: Instant,
    /// It could not be reached: not tried again before this.
    broken_until: Option<Instant>,
}

#[cfg(feature = "unstable-http3")]
impl fmt::Debug for Http3 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Http3")
    }
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
            #[cfg(feature = "unstable-http3")]
            http3: None,
        }
    }
}

impl ClientBuilder {
    /// How long a request may take in all (default 30 s): waiting for a
    /// connection, connecting, sending it, reading the whole response, and
    /// following redirects. [`RequestBuilder::timeout`] changes it for one
    /// request.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = crate::bounded(timeout);
        self
    }

    /// How long opening a connection may take, TLS included (default 10 s).
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = crate::bounded(timeout);
        self
    }

    /// How long an unused connection is kept open (default 60 s).
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = crate::bounded(timeout);
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

    /// Speaks HTTP/3 to the `https` origins that say they do (off by
    /// default). A response over TCP whose `Alt-Svc` header field names
    /// `h3` on the origin's own host (`alt-svc: h3=":443"; ma=86400`) makes
    /// the requests that follow, for as long as it says and a week at
    /// most, go over QUIC to that UDP port; an alternative on another host
    /// is not followed. If the alternative does not answer within 2 s (or
    /// [`connect_timeout`](Self::connect_timeout), when that is shorter)
    /// the request goes over TCP as before, and HTTP/3 is left alone for
    /// that origin for five minutes. Not through a proxy.
    ///
    /// `quic` is as for
    /// [`http3_prior_knowledge`](Self::http3_prior_knowledge), which this
    /// replaces when both are called: the last one counts.
    #[cfg(feature = "unstable-http3")]
    pub fn http3(mut self, quic: giong_quic::ClientConfig) -> Self {
        self.http3 = Some(Http3 {
            quic,
            prior_knowledge: false,
        });
        self
    }

    /// Speaks HTTP/3 to `https` URLs from the first packet, without asking
    /// ("prior knowledge", off by default): for servers known to accept it,
    /// such as giong-http's own `Server::serve_h3`. Every request to an
    /// `https` origin then goes over QUIC, on one connection for each
    /// origin; nothing falls back to TCP. Not through a proxy, where
    /// requests stay on HTTP/1.1 and HTTP/2. A request with a streamed body
    /// ([`RequestBuilder::streamed_body`]) is refused with
    /// [`ErrorKind::Request`], since HTTP/3 requests carry bodies held in
    /// memory.
    ///
    /// `quic` carries the TLS settings, which must offer the ALPN protocol
    /// `h3`, and the transport's: its idle timeout is what closes an unused
    /// connection, and its stream limits and windows bound what a server
    /// can make the client hold. [`tls`](Self::tls) is not used for these
    /// requests; [`connect_timeout`](Self::connect_timeout) is.
    ///
    /// With early data on in `quic` (`giong_quic::ClientConfig::early_data`),
    /// a new connection to a server the client holds a session ticket for
    /// sends its first requests in 0-RTT data, a round trip sooner. Since
    /// early data can be replayed, only GET, HEAD and OPTIONS requests
    /// without a body go in it; the others wait for the handshake. A
    /// request the server refuses to take early (its 0-RTT data refused, or
    /// `425 Too Early`) is sent again once the handshake is over.
    #[cfg(feature = "unstable-http3")]
    pub fn http3_prior_knowledge(mut self, quic: giong_quic::ClientConfig) -> Self {
        self.http3 = Some(Http3 {
            quic,
            prior_knowledge: true,
        });
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
                #[cfg(feature = "unstable-http3")]
                h3: H3Pool::default(),
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
    #[cfg(feature = "unstable-http3")]
    h3: H3Pool,
}

/// The HTTP/3 connections of a client: one for each origin, on a QUIC
/// endpoint for each address family, bound when first needed.
#[cfg(feature = "unstable-http3")]
#[derive(Default)]
struct H3Pool {
    origins: RefCell<HashMap<url::Origin, H3Slot>>,
    /// What origins advertised with `Alt-Svc`.
    alternatives: RefCell<HashMap<url::Origin, Alternative>>,
    v4: OnceCell<giong_quic::Endpoint>,
    v6: OnceCell<giong_quic::Endpoint>,
}

#[cfg(feature = "unstable-http3")]
enum H3Slot {
    /// A request is connecting: the others wait for it at the gate.
    Opening(Rc<Gate>),
    Open(h3::Connection),
}

/// Where requests wait while another opens the connection they will share.
#[cfg(feature = "unstable-http3")]
#[derive(Default)]
struct Gate {
    open: Cell<bool>,
    /// Why the opener could not connect: the requests that waited for it
    /// give up with it at once, rather than each trying again in turn.
    failed: RefCell<Option<Error>>,
    waiting: RefCell<Vec<std::task::Waker>>,
}

#[cfg(feature = "unstable-http3")]
impl Gate {
    async fn passed(&self) {
        std::future::poll_fn(|cx| {
            if self.open.get() {
                return std::task::Poll::Ready(());
            }
            self.waiting.borrow_mut().push(cx.waker().clone());
            std::task::Poll::Pending
        })
        .await;
    }
}

/// The request that opens an origin's HTTP/3 connection. Dropped, whether
/// it connected or not, it lets the waiting requests look again.
#[cfg(feature = "unstable-http3")]
struct H3Opening<'a> {
    pool: &'a H3Pool,
    origin: &'a url::Origin,
    gate: Rc<Gate>,
}

#[cfg(feature = "unstable-http3")]
impl Drop for H3Opening<'_> {
    fn drop(&mut self) {
        let mut origins = self.pool.origins.borrow_mut();
        if matches!(origins.get(self.origin), Some(H3Slot::Opening(g)) if Rc::ptr_eq(g, &self.gate))
        {
            origins.remove(self.origin);
        }
        drop(origins);
        self.gate.open.set(true);
        for waker in self.gate.waiting.borrow_mut().drain(..) {
            waker.wake();
        }
    }
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
            source: None,
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

    /// Opens a connection to `url`'s origin before the first request needs
    /// it, and returns the version it speaks: DNS, TCP, the proxy tunnel
    /// and the TLS handshake are done, with the client's usual settings,
    /// and no request is sent. Only the origin of `url` (scheme, host and
    /// port) is used.
    ///
    /// For a service that should not make its first requests wait for a
    /// handshake: call it when the service starts, alongside accepting
    /// connections, and again whenever [`ping_idle_http2`](Self::ping_idle_http2)
    /// had to close a connection.
    ///
    /// * An origin that speaks HTTP/2 gets one connection, which joins the
    ///   pool as soon as it can take requests. Requests made while it is
    ///   being opened wait for it and then share it, rather than each
    ///   opening its own; their wait is bounded by
    ///   [`connect_timeout`](ClientBuilder::connect_timeout) for the
    ///   connection and by their own timeout in all.
    /// * Should ALPN pick HTTP/1.1, the connection is put among the idle
    ///   ones for the next request, the requests that were waiting for it
    ///   open their own, and this returns [`Version::Http11`]. So does an
    ///   `http` origin without
    ///   [`http2_prior_knowledge`](ClientBuilder::http2_prior_knowledge).
    /// * Should the connection fail, this returns the error
    ///   ([`ErrorKind::Connect`]), and the first request waiting for it
    ///   opens a connection of its own. No request is sent before it has a
    ///   connection, so none is sent twice.
    ///
    /// With HTTP/3 (`ClientBuilder::http3_prior_knowledge`, or an `Alt-Svc`
    /// alternative the client already knows), the origin gets its QUIC
    /// connection instead, and this returns [`Version::Http3`]. An
    /// alternative that does not answer within the short time requests give
    /// it leaves the origin to TCP, as it leaves requests.
    ///
    /// Returns at once when the origin has a connection already: an HTTP/2
    /// one that can take requests ([`Version::Http2`]), or an idle HTTP/1.1
    /// one ([`Version::Http11`]). When the origin has as many connections
    /// open as [`max_connections_per_host`](ClientBuilder::max_connections_per_host)
    /// allows, it waits in line like a request. Everything is bounded by the
    /// client's [`timeout`](ClientBuilder::timeout) ([`ErrorKind::Timeout`]).
    ///
    /// Cancel safe: if the future is dropped, nothing was sent; a
    /// connection being opened is abandoned, and the first request waiting
    /// for it opens one of its own.
    pub async fn preconnect(&self, url: &str) -> Result<Version, Error> {
        let url = Url::parse(url).map_err(Error::request)?;
        let config = &self.shared.config;
        let route = self.route(&url);
        let run = async {
            // An origin requests go to over HTTP/3 gets its QUIC connection.
            // A known alternative is tried as requests try it: briefly, with
            // TCP to fall back on.
            #[cfg(feature = "unstable-http3")]
            if let (Some(http3), true, None) = (&config.http3, url.https, &route.proxy) {
                let origin = url.origin();
                if http3.prior_knowledge {
                    self.h3_connection(&http3.quic, &origin, origin.port, config.connect_timeout)
                        .await?;
                    return Ok(Version::Http3);
                }
                if let Some(port) = self.h3_alternative(&origin) {
                    let within = config.connect_timeout.min(ALTERNATIVE_TIMEOUT);
                    if self
                        .h3_connection(&http3.quic, &origin, port, within)
                        .await
                        .is_ok()
                    {
                        return Ok(Version::Http3);
                    }
                }
            }
            loop {
                let pool = self.host(&route);
                if pool::has_usable_h2(&pool) {
                    return Ok(Version::Http2);
                }
                match self.checkout(&route, pool, false).await? {
                    Checkout::Retry => continue,
                    Checkout::H2(_) | Checkout::Shared(_) => return Ok(Version::Http2),
                    Checkout::H1(mut lease) => {
                        // Back unused: its idle time is not reset.
                        if let Some(conn) = lease.conn.take() {
                            pool::put_back(&lease.pool, conn, config.idle_timeout);
                        }
                        return Ok(Version::Http11);
                    }
                }
            }
        };
        match giong::time::timeout(config.timeout, run).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "preconnecting to {} did not finish within {:?}; if the server is known \
                     to be this slow, raise ClientBuilder::timeout (and \
                     ClientBuilder::connect_timeout, for the handshake)",
                    url.origin(),
                    config.timeout
                ),
            )),
        }
    }

    /// Checks that the idle HTTP/2 connections to `url`'s origin still
    /// work: sends each one a PING and waits up to `deadline` for the
    /// server to acknowledge it. Returns how many did. Only the origin of
    /// `url` is used. Nothing happens unless this is called: the client
    /// never pings on its own.
    ///
    /// For a connection that may have died silently while idle (a NAT or
    /// load balancer dropped it, the server vanished without closing it),
    /// which a request would otherwise find only when its own timeout ran
    /// out. A request sent while the probe is running goes on the
    /// connection as usual.
    ///
    /// * `Ok(n)`: every connection probed acknowledged its PING, `n` of
    ///   them; an acknowledged connection counts as used, so its
    ///   [`idle_timeout`](ClientBuilder::idle_timeout) starts again. `Ok(0)`
    ///   means there was no idle HTTP/2 connection to probe (none open,
    ///   all busy with requests, or the origin speaks HTTP/1.1): call
    ///   [`preconnect`](Self::preconnect) to open one.
    /// * [`ErrorKind::Timeout`]: a connection did not acknowledge in time.
    ///   If no request was sent on it during the probe, it is closed at
    ///   once and takes no more requests; one that carried a request
    ///   meanwhile (still going or already answered) is left open, and a
    ///   request still going has its own timeout. The message says how
    ///   many of each. Connections that acknowledged stay.
    /// * [`ErrorKind::Closed`]: a connection ended while it was probed, or
    ///   the server said GOAWAY on it, even alongside the acknowledgement;
    ///   it takes no more requests and leaves the pool. The message says
    ///   why it ended.
    ///
    /// A probe in flight keeps the connection from being closed for being
    /// idle: the idle timeout is not checked until the PING is
    /// acknowledged or given up.
    ///
    /// After an error, the next request opens a new connection; call
    /// [`preconnect`](Self::preconnect) to open it now. A `deadline` of zero
    /// is refused ([`ErrorKind::Request`]); the client's timeouts do not
    /// apply, `deadline` does.
    ///
    /// Cancel safe: if the future is dropped, no connection is closed and
    /// acknowledgements that arrive later are ignored.
    pub async fn ping_idle_http2(&self, url: &str, deadline: Duration) -> Result<usize, Error> {
        let url = Url::parse(url).map_err(Error::request)?;
        if deadline.is_zero() {
            return Err(Error::request(
                "a PING deadline of zero cannot be met; give ping_idle_http2 the longest time \
                 a healthy server may take to answer, such as a few round trips",
            ));
        }
        let Some(pool) = self.shared.hosts.borrow().get(&self.route(&url)).cloned() else {
            return Ok(0);
        };
        let mut probes: Vec<_> = pool::idle_h2(&pool)
            .into_iter()
            .filter_map(|(id, conn)| {
                // Requests sent from here on show up in `opened`.
                let opened = conn.opened();
                conn.ping().map(|ping| (id, conn, opened, ping, None))
            })
            .collect();
        let all = std::future::poll_fn(|cx| {
            let mut waiting = false;
            for (_, _, _, ping, result) in &mut probes {
                if result.is_none() {
                    match ping.poll_acked(cx) {
                        std::task::Poll::Ready(r) => *result = Some(r),
                        std::task::Poll::Pending => waiting = true,
                    }
                }
            }
            if waiting {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(())
            }
        });
        let _ = giong::time::timeout(deadline, all).await;
        let origin = url.origin();
        let (mut answered, mut retired, mut busy, mut ended) = (0, 0, 0, None);
        let probed = probes.len();
        for (id, conn, opened, ping, result) in probes {
            drop(ping);
            match result {
                // Acknowledged, but the server said GOAWAY too (often in
                // the same read): it takes no more requests.
                Some(Ok(())) if !conn.is_usable() => ended = conn.unusable_because(),
                Some(Ok(())) => answered += 1,
                Some(Err(why)) => ended = Some(why),
                // Checked and closed in one go: no request can start on it
                // in between. One that carried a request during the probe,
                // even one already over, is not judged by the PING alone.
                None if conn.streams() == 0 && conn.opened() == opened => {
                    conn.retire(&format!(
                        "the server did not acknowledge a PING within {deadline:?}"
                    ));
                    pool::h2_closed(&pool, id);
                    retired += 1;
                }
                None => busy += 1,
            }
        }
        if retired + busy > 0 {
            let mut what = Vec::new();
            if retired > 0 {
                what.push(format!(
                    "{retired} closed (the next request opens a new connection; \
                     Client::preconnect opens it now)"
                ));
            }
            if busy > 0 {
                what.push(format!(
                    "{busy} left open because a request was sent on it meanwhile (a request \
                     still going has its own timeout)"
                ));
            }
            return Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "{origin}: {} of {probed} idle HTTP/2 connections did not acknowledge a PING \
                     within {deadline:?}: {}; if the server is known to answer this slowly, give \
                     ping_idle_http2 a longer deadline",
                    retired + busy,
                    what.join(", and ")
                ),
            ));
        }
        if let Some(why) = ended {
            return Err(Error::new(
                ErrorKind::Closed,
                format!(
                    "{origin}: an idle HTTP/2 connection ended while it was pinged ({why}); the \
                     next request opens a new one (Client::preconnect opens it now)"
                ),
            ));
        }
        Ok(answered)
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
        // An HTTP/3 connection closes by its own idle timeout, QUIC's.
        #[cfg(feature = "unstable-http3")]
        shared
            .h3
            .origins
            .borrow_mut()
            .retain(|_, slot| !matches!(slot, H3Slot::Open(conn) if !conn.is_open()));
    }

    /// A connection on `route`: an idle one unless `fresh`, otherwise a
    /// new one, waiting in line while the origin has as many open as
    /// allowed. A new connection that speaks HTTP/2 joins the pool, shared.
    async fn checkout(
        &self,
        route: &Route,
        pool: pool::Shared,
        fresh: bool,
    ) -> Result<Checkout, Error> {
        let config = &self.shared.config;
        let prior_knowledge =
            config.http2_prior_knowledge && !route.origin.https && route.proxy.is_none();
        let taken = pool::acquire(
            &pool,
            config.max_connections_per_host,
            config.idle_timeout,
            fresh,
            prior_knowledge,
            config.http2 && route.origin.https,
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
        source: &mut Option<StreamBody>,
        trailers: bool,
    ) -> Result<Pending, Error> {
        let config = &self.shared.config;
        let streamed = source.is_some();
        let route = self.route(url);
        let what = url.describe(method);
        let decompress = config.decompress
            && !headers
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("accept-encoding"));
        // A server spoken to by prior knowledge may have no TCP: refused
        // rather than sent there.
        #[cfg(feature = "unstable-http3")]
        if let (Some(http3), true, None, true) = (&config.http3, url.https, &route.proxy, streamed)
        {
            if http3.prior_knowledge {
                return Err(Error::request(format!(
                    "{what}: a streamed request body cannot go over HTTP/3 yet, and this client \
                     speaks HTTP/3 to https origins by prior knowledge; send the body from \
                     memory (RequestBuilder::body), or use a client without \
                     http3_prior_knowledge"
                )));
            }
        }
        #[cfg(feature = "unstable-http3")]
        // A streamed request body goes over HTTP/1.1 or HTTP/2: the HTTP/3
        // client sends bodies it has whole.
        if let (Some(http3), true, None, false) = (&config.http3, url.https, &route.proxy, streamed)
        {
            let origin = url.origin();
            let to = if http3.prior_knowledge {
                Some((origin.port, config.connect_timeout))
            } else {
                // An alternative is used once it is known to answer:
                // until then the request has TCP to go over.
                let within = config.connect_timeout.min(ALTERNATIVE_TIMEOUT);
                match self.h3_alternative(&origin) {
                    Some(port) => self
                        .h3_connection(&http3.quic, &origin, port, within)
                        .await
                        .ok()
                        .map(|_| (port, within)),
                    None => None,
                }
            };
            if let Some(to) = to {
                return self
                    .execute_h3(
                        http3, to, method, url, headers, body, decompress, trailers, what,
                    )
                    .await;
            }
        }
        let mut request = None;
        let mut fresh = false;
        let mut refused = 0;
        loop {
            // An HTTP/2 connection with room takes the request at once.
            let pool = self.host(&route);
            let checkout = match pool::h2_with_room(&pool) {
                Some(conn) => Checkout::Shared(conn),
                None => self.checkout(&route, pool, fresh).await?,
            };
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
                            if streamed {
                                Payload::Streamed
                            } else {
                                Payload::Whole(body)
                            },
                            &config.user_agent,
                            route.absolute_form(),
                            decompress,
                        )
                    });
                    let reused = lease.reused;
                    let mut pulled = false;
                    let sent = match source.as_ref() {
                        Some(source) => {
                            lease
                                .conn()
                                .send_streamed(
                                    request.clone(),
                                    source,
                                    &mut pulled,
                                    method == "HEAD",
                                    config.max_head_size,
                                    &what,
                                )
                                .await
                        }
                        None => {
                            lease
                                .conn()
                                .send(
                                    request,
                                    reused,
                                    method == "HEAD",
                                    config.max_head_size,
                                    &what,
                                )
                                .await
                        }
                    };
                    if pulled || sent.is_ok() {
                        // Dropped, which tells its sender that nothing more
                        // of the body is taken.
                        *source = None;
                    }
                    match sent {
                        Ok(answer) => {
                            #[cfg(feature = "unstable-http3")]
                            if route.proxy.is_none() {
                                self.note_alt_svc(url, &answer.head, answer.body.head_bytes());
                            }
                            return Ok(Pending::new(
                                Some(lease),
                                Body::H1(answer.body),
                                answer.head,
                                url,
                                what,
                                decompress,
                            ));
                        }
                        // A streamed body nothing was taken from is whole:
                        // the request can be sent again as it is. One that
                        // was taken from cannot be, whatever its method.
                        Err(Failure::NothingReceived(_))
                            if reused
                                && if streamed {
                                    source.is_some()
                                } else {
                                    is_idempotent(method)
                                } =>
                        {
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
            let fields = h2_fields(
                method,
                headers,
                body,
                &config.user_agent,
                decompress,
                streamed,
                trailers,
            );
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
            let Some(mut stream) = conn.send(&req, source) else {
                // Its room went to another request meanwhile.
                continue;
            };
            match stream.response().await {
                Ok(()) => {
                    let fields = stream.take_fields();
                    let head = h2_head(stream.head(), fields);
                    #[cfg(feature = "unstable-http3")]
                    if route.proxy.is_none() {
                        self.note_alt_svc(url, &head, &stream.head().bytes);
                    }
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
                    if streamed {
                        *source = stream.unused_source();
                        if source.is_none() {
                            return Err(Error::new(
                                ErrorKind::Closed,
                                format!(
                                    "{what}: {m}; part of the streamed request body had \
                                     been sent, so the request is not sent again by itself"
                                ),
                            ));
                        }
                    }
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
                    if streamed {
                        *source = stream.unused_source();
                        if source.is_none() {
                            return Err(not_sent_again(e, method));
                        }
                    } else if !is_idempotent(method) {
                        return Err(not_sent_again(e, method));
                    }
                    fresh = true;
                }
                Err(e) => return Err(h2_error(e, &what)),
            }
        }
    }

    /// Sends a request over HTTP/3. One the server did not process is sent
    /// again, whatever its method; an idempotent one that meets a
    /// connection closing under it is sent once more on a new one.
    #[cfg(feature = "unstable-http3")]
    #[allow(clippy::too_many_arguments)]
    async fn execute_h3(
        &self,
        http3: &Http3,
        (port, connect_timeout): (u16, Duration),
        method: &str,
        url: &Url,
        headers: &[(String, String)],
        body: Option<&[u8]>,
        decompress: bool,
        trailers: bool,
        what: String,
    ) -> Result<Pending, Error> {
        let config = &self.shared.config;
        let origin = url.origin();
        let fields = h2_fields(
            method,
            headers,
            body,
            &config.user_agent,
            decompress,
            false,
            trailers,
        );
        let authority = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case("host"))
            .map_or_else(|| url.host_header(), |(_, v)| v.clone());
        let req = h2::Request {
            method,
            scheme: "https",
            authority: &authority,
            path: &url.target,
            fields: &fields,
            body,
            head_request: method == "HEAD",
        };
        let mut refused = 0;
        let mut again = false;
        let mut too_early = false;
        loop {
            let (conn, reused) = self
                .h3_connection(&http3.quic, &origin, port, connect_timeout)
                .await?;
            let response = async {
                let mut stream = conn.send(&req).await?;
                stream.response().await?;
                Ok(stream)
            };
            match response.await {
                // 425 Too Early: the server will take the request once the
                // handshake is over (RFC 8470, section 5.2).
                Ok(stream) if stream.head().status == 425 && stream.was_early() && !too_early => {
                    too_early = true;
                    drop(stream);
                    conn.handshake().await;
                }
                Ok(mut stream) => {
                    let fields = stream.take_fields();
                    let head = h2_head(stream.head(), fields);
                    self.note_alt_svc(url, &head, &stream.head().bytes);
                    return Ok(Pending::new(
                        None,
                        Body::H3(Box::new(stream)),
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
                }) if reused && !again => {
                    let e = Error::new(ErrorKind::Closed, format!("{what}: {message}"));
                    if !is_idempotent(method) {
                        return Err(not_sent_again(e, method));
                    }
                    again = true;
                }
                Err(e) => return Err(h3_error(e, &what)),
            }
        }
    }

    /// The HTTP/3 connection to `origin`, and whether it was there
    /// already. The first request to an origin opens it and the others
    /// wait for that, so that a burst of requests shares one connection.
    /// Cancel safe: a request dropped while it connects lets the next one
    /// try.
    #[cfg(feature = "unstable-http3")]
    async fn h3_connection(
        &self,
        quic: &giong_quic::ClientConfig,
        origin: &url::Origin,
        port: u16,
        connect_timeout: Duration,
    ) -> Result<(h3::Connection, bool), Error> {
        let pool = &self.shared.h3;
        loop {
            let waiting = {
                let mut origins = pool.origins.borrow_mut();
                match origins.get(origin) {
                    Some(H3Slot::Open(conn)) if conn.is_open() => return Ok((conn.clone(), true)),
                    Some(H3Slot::Opening(gate)) => Err(gate.clone()),
                    _ => {
                        let gate = Rc::new(Gate::default());
                        origins.insert(origin.clone(), H3Slot::Opening(gate.clone()));
                        Ok(gate)
                    }
                }
            };
            let gate = match waiting {
                Ok(gate) => gate,
                Err(gate) => {
                    gate.passed().await;
                    if let Some(e) = gate.failed.borrow().clone() {
                        return Err(e);
                    }
                    continue;
                }
            };
            let opening = H3Opening { pool, origin, gate };
            // Boxed for the reason `checkout` boxes `Conn::open`.
            let started = Instant::now();
            let opened = Box::pin(self.h3_open(quic, origin, port, connect_timeout)).await;
            // With a ticket, a connection opens without a packet from the
            // server (0-RTT). An origin's advertised alternative is used
            // only once the server has answered, within the same connect
            // timeout, so that a network that blocks UDP sends the
            // requests over TCP at once rather than after their timeout.
            // With prior knowledge there is no TCP to go to, and 0-RTT is
            // kept.
            let prior_knowledge = self
                .shared
                .config
                .http3
                .as_ref()
                .is_some_and(|http3| http3.prior_knowledge);
            let opened = match opened {
                Ok(conn) if !prior_knowledge => {
                    let left = connect_timeout.saturating_sub(started.elapsed());
                    match giong::time::timeout(left, conn.handshake()).await {
                        Ok(()) if conn.is_open() => Ok(conn),
                        Ok(()) => Err(Error::new(
                            ErrorKind::Connect,
                            format!(
                                "the HTTP/3 handshake with {origin} (UDP port {port}) failed; \
                                 the request goes over TCP"
                            ),
                        )),
                        Err(_) => Err(Error::new(
                            ErrorKind::Timeout,
                            format!(
                                "no answer from {origin} over HTTP/3 (UDP port {port}) within \
                                 {connect_timeout:?}; the request goes over TCP"
                            ),
                        )),
                    }
                }
                other => other,
            };
            let conn = match opened {
                Ok(conn) => conn,
                Err(e) => {
                    // An alternative that does not answer is left alone
                    // for a while; the requests go over TCP meanwhile.
                    if let Some(alternative) = pool.alternatives.borrow_mut().get_mut(origin) {
                        alternative.broken_until = Some(Instant::now() + ALTERNATIVE_BROKEN_FOR);
                    }
                    *opening.gate.failed.borrow_mut() = Some(e.clone());
                    return Err(e);
                }
            };
            pool.origins
                .borrow_mut()
                .insert(origin.clone(), H3Slot::Open(conn.clone()));
            drop(opening);
            return Ok((conn, false));
        }
    }

    /// Connects to `origin` over QUIC, trying its addresses in turn, and
    /// starts HTTP/3 on the connection.
    #[cfg(feature = "unstable-http3")]
    async fn h3_open(
        &self,
        quic: &giong_quic::ClientConfig,
        origin: &url::Origin,
        port: u16,
        connect_timeout: Duration,
    ) -> Result<h3::Connection, Error> {
        let failed = |e: io::Error| {
            Error::io(
                ErrorKind::Connect,
                format!(
                    "cannot connect to {origin} over HTTP/3 (does the server speak HTTP/3 on \
                     UDP port {port}, and is UDP allowed on the way?)"
                ),
                e,
            )
        };
        let connect = async {
            let addrs = giong::net::lookup_host((origin.host.as_str(), port))
                .await
                .map_err(failed)?;
            self.h3_open_any(quic, origin, &addrs, connect_timeout)
                .await
                .map_err(failed)
        };
        match giong::time::timeout(connect_timeout, connect).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "connecting to {origin} over HTTP/3 took longer than {connect_timeout:?}; \
                     check that the server speaks HTTP/3 on UDP port {port} and that UDP is \
                     allowed on the way, or raise ClientBuilder::connect_timeout"
                ),
            )),
        }
    }

    /// Connects to the first of `addrs` that answers, each in its turn.
    #[cfg(feature = "unstable-http3")]
    async fn h3_open_any(
        &self,
        quic: &giong_quic::ClientConfig,
        origin: &url::Origin,
        addrs: &[std::net::SocketAddr],
        connect_timeout: Duration,
    ) -> io::Result<h3::Connection> {
        let config = &self.shared.config;
        let pool = &self.shared.h3;
        // Nothing answers a QUIC packet sent to an address that does not
        // listen, so each address gets its share of the time and the next
        // one is tried when it has passed.
        let share = connect_timeout / addrs.len().max(1) as u32;
        let mut last = None;
        for &addr in addrs {
            let (slot, any) = if addr.is_ipv4() {
                (&pool.v4, "0.0.0.0:0")
            } else {
                (&pool.v6, "[::]:0")
            };
            let endpoint = match slot.get() {
                Some(endpoint) => endpoint,
                None => {
                    let any = any.parse().expect("a literal address");
                    match giong_quic::Endpoint::client(any) {
                        Ok(endpoint) => slot.get_or_init(|| endpoint),
                        Err(e) => {
                            last = Some(e);
                            continue;
                        }
                    }
                }
            };
            let made = async {
                let conn = endpoint.connect_early(addr, &origin.host, quic).await?;
                let conn = h3::Connection::start(conn, config.max_head_size).await?;
                // With a ticket the connection opens before any packet
                // comes back. When there are other addresses to try, this
                // one counts only once it has answered, or a dead first
                // address (an unreachable IPv6 one before a working IPv4
                // one, say) would be the only one ever tried.
                if addrs.len() > 1 {
                    conn.handshake().await;
                    if !conn.is_open() {
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            format!("the handshake with {addr} failed"),
                        ));
                    }
                }
                Ok(conn)
            };
            match giong::time::timeout(share, made).await {
                Ok(Ok(conn)) => return Ok(conn),
                Ok(Err(e)) => last = Some(e),
                Err(_) => {
                    last = Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("no answer from {addr} within {share:?}"),
                    ));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "the host name has no address")
        }))
    }

    /// The UDP port `origin` advertised HTTP/3 on, while that holds and the
    /// alternative is not known to be out of reach.
    #[cfg(feature = "unstable-http3")]
    fn h3_alternative(&self, origin: &url::Origin) -> Option<u16> {
        let mut alternatives = self.shared.h3.alternatives.borrow_mut();
        let alternative = alternatives.get(origin)?;
        let now = Instant::now();
        if alternative.until <= now {
            alternatives.remove(origin);
            return None;
        }
        if alternative.broken_until.is_some_and(|until| now < until) {
            return None;
        }
        Some(alternative.port)
    }

    /// Reads what a response says about its origin's HTTP/3 (`Alt-Svc`),
    /// for a client that follows it.
    #[cfg(feature = "unstable-http3")]
    fn note_alt_svc(&self, url: &Url, head: &Head, bytes: &[u8]) {
        let shared = &self.shared;
        if !shared
            .config
            .http3
            .as_ref()
            .is_some_and(|http3| !http3.prior_knowledge && url.https)
        {
            return;
        }
        // Every field line, as one comma-separated value (RFC 9110
        // section 5.3).
        let mut value = Vec::new();
        for (n, v) in &head.fields {
            if bytes[n.clone()].eq_ignore_ascii_case(b"alt-svc") {
                if !value.is_empty() {
                    value.extend_from_slice(b", ");
                }
                value.extend_from_slice(&bytes[v.clone()]);
            }
        }
        if value.is_empty() {
            return;
        }
        let mut alternatives = shared.h3.alternatives.borrow_mut();
        match alt_svc::parse(&value) {
            alt_svc::AltSvc::Malformed => {}
            // A valid value replaces what was known (RFC 7838 section 3).
            alt_svc::AltSvc::Clear | alt_svc::AltSvc::NoH3 => {
                alternatives.remove(&url.origin());
            }
            alt_svc::AltSvc::H3 { port, max_age } => {
                let until = Instant::now() + max_age;
                match alternatives.get_mut(&url.origin()) {
                    // Said again: it holds for longer, and what is known
                    // of its reach stays.
                    Some(known) if known.port == port => known.until = until,
                    _ => {
                        alternatives.insert(
                            url.origin(),
                            Alternative {
                                port,
                                until,
                                broken_until: None,
                            },
                        );
                    }
                }
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
        mut source: Option<StreamBody>,
        trailers: bool,
    ) -> Result<Pending, Error> {
        let config = &self.shared.config;
        let max = config.redirects;
        let streamed = source.is_some();
        let mut hops = 0;
        loop {
            let pending = self
                .execute(
                    &method,
                    &url,
                    &headers,
                    body.as_deref(),
                    &mut source,
                    trailers,
                )
                .await?;
            let status = pending.head.status;
            // A streamed body cannot be sent a second time.
            if max == 0 || streamed || !matches!(status, 301 | 302 | 303 | 307 | 308) {
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
    /// `METHOD url`, for messages; the URL is its tail, from `url_at`.
    what: String,
    url_at: usize,
}

/// The body of a response, over either protocol.
enum Body {
    H1(conn::Body),
    H2(h2::Stream),
    #[cfg(feature = "unstable-http3")]
    H3(Box<h3::Stream>),
}

impl Body {
    fn head_bytes(&self) -> &[u8] {
        match self {
            Body::H1(b) => b.head_bytes(),
            Body::H2(s) => &s.head().bytes,
            #[cfg(feature = "unstable-http3")]
            Body::H3(s) => &s.head().bytes,
        }
    }

    /// The trailer fields the body ended with, once it was read to its end.
    fn trailers(&self) -> Option<&Trailers> {
        match self {
            Body::H1(b) => b.trailers(),
            Body::H2(s) => s.trailers(),
            #[cfg(feature = "unstable-http3")]
            Body::H3(s) => s.trailers(),
        }
    }

    /// The length of the body, when known.
    fn length(&self) -> Option<u64> {
        match self {
            Body::H1(b) => b.length(),
            Body::H2(s) => s.body_length(),
            #[cfg(feature = "unstable-http3")]
            Body::H3(s) => s.body_length(),
        }
    }

    /// The buffer holding the head and what came with it, when that is
    /// the whole body.
    fn take_with_head(&mut self) -> Option<Vec<u8>> {
        match self {
            Body::H1(b) => b.take_with_head(),
            Body::H2(_) => None,
            #[cfg(feature = "unstable-http3")]
            Body::H3(_) => None,
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
            #[cfg(feature = "unstable-http3")]
            Body::H3(s) => s.read(out, limit).await.map_err(|e| h3_error(e, what)),
        }
    }

    /// The body has been read to its end.
    fn is_done(&self) -> bool {
        match self {
            Body::H1(b) => b.is_done(),
            Body::H2(s) => s.is_done(),
            #[cfg(feature = "unstable-http3")]
            Body::H3(s) => s.is_done(),
        }
    }

    /// The HTTP/1.1 connection can carry another request.
    fn reusable(&self) -> bool {
        match self {
            Body::H1(b) => b.reusable(),
            Body::H2(_) => false,
            #[cfg(feature = "unstable-http3")]
            Body::H3(_) => false,
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
        // The method is a token: the first space ends it.
        let url_at = what.find(' ').map_or(0, |i| i + 1);
        debug_assert_eq!(what[url_at..], url.text());
        Self {
            lease,
            head,
            body,
            inflate,
            what,
            url_at,
        }
    }

    fn url(&self) -> &str {
        &self.what[self.url_at..]
    }

    fn version(&self) -> Version {
        match (&self.body, self.head.minor) {
            (Body::H2(_), _) => Version::Http2,
            #[cfg(feature = "unstable-http3")]
            (Body::H3(_), _) => Version::Http3,
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
            #[cfg(feature = "unstable-http3")]
            Body::H3(s) => s.is_done(),
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
            #[cfg(feature = "unstable-http3")]
            Body::H3(s) => s.cancel(),
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
            url: self.what,
            url_at: self.url_at,
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
    e.advise(format!(
        "the connection had been used before, so the server probably closed it as the \
         request was sent. A {method} request is not sent again by itself, because the server \
         may have processed it: retry if that is safe"
    ))
}

/// The fields of an HTTP/2 request after its pseudo-headers: the same as
/// over HTTP/1.1, in lowercase, with `Host` turned into `:authority`.
fn h2_fields(
    method: &str,
    headers: &[(String, String)],
    body: Option<&[u8]>,
    user_agent: &str,
    accept_encoding: bool,
    streamed: bool,
    trailers: bool,
) -> Vec<(String, String)> {
    let mut fields = Vec::with_capacity(headers.len() + 4);
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
    if trailers {
        // The response is read as a stream, trailers included
        // (StreamingResponse::trailers), and some servers (gRPC) only send
        // them to a client that says it reads them.
        fields.push(("te".to_owned(), "trailers".to_owned()));
    }
    if streamed {
        // No length: the body ends with the stream.
    } else if body.is_some() || matches!(method, "POST" | "PUT" | "PATCH") {
        fields.push((
            "content-length".to_owned(),
            body.map_or(0, <[u8]>::len).to_string(),
        ));
    }
    fields
}

/// The head of an HTTP/2 response, as the rest of the client reads heads.
fn h2_head(head: &h2::ResponseHead, fields: Vec<(Range<usize>, Range<usize>)>) -> Head {
    Head {
        status: head.status,
        minor: 1,
        fields,
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

#[cfg(feature = "unstable-http3")]
fn h3_error(e: h2::StreamError, what: &str) -> Error {
    match e {
        h2::StreamError::Refused(m)
        | h2::StreamError::Reset(m)
        | h2::StreamError::Closed { message: m, .. } => {
            Error::new(ErrorKind::Closed, format!("{what}: {m}"))
        }
        h2::StreamError::Protocol(m) => Error::new(
            ErrorKind::Protocol,
            format!("{what}: the server sent an invalid HTTP/3 response: {m}"),
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

/// The body of a request being encoded for HTTP/1.1.
#[derive(Clone, Copy)]
enum Payload<'a> {
    /// In memory, if there is one: sent with the head, with its length.
    Whole(Option<&'a [u8]>),
    /// Produced piece by piece: sent chunked, after the head.
    Streamed,
}

fn encode_request(
    method: &str,
    url: &Url,
    headers: &[(String, String)],
    body: Payload<'_>,
    user_agent: &str,
    proxy: Option<&proxy::Server>,
    accept_encoding: bool,
) -> Vec<u8> {
    let streamed = matches!(body, Payload::Streamed);
    let body = match body {
        Payload::Whole(body) => body,
        Payload::Streamed => None,
    };
    let body_len = body.map_or(0, <[u8]>::len);
    let mut out = Vec::with_capacity(256 + url.target.len() + body_len);
    let has = |name: &str| headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(name));
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    if proxy.is_some() {
        // Absolute form, for a proxy (RFC 9112 section 3.2.2).
        out.extend_from_slice(b"http://");
        url.push_host_header(&mut out);
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
        url.push_host_header(&mut out);
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
    if streamed {
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    } else if body.is_some() || matches!(method, "POST" | "PUT" | "PATCH") {
        out.extend_from_slice(b"Content-Length: ");
        crate::response::push_number(&mut out, body_len);
        out.extend_from_slice(b"\r\n");
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
    /// The receiving half of a streamed body.
    source: Option<StreamBody>,
    timeout: Option<Duration>,
    error: Option<Error>,
}

impl RequestBuilder {
    /// Adds a header. `Host` and `User-Agent` replace the client's own;
    /// the headers that frame the message (`Content-Length`,
    /// `Transfer-Encoding`, `Connection`...) are the client's to set, and
    /// setting one is an error.
    ///
    /// The value is anything that reads as text, as for the server's
    /// [`Response::header`](crate::Response::header): a `&str`, a `String`,
    /// or `format!(..)` without `&`.
    pub fn header(mut self, name: &str, value: impl AsRef<str>) -> Self {
        let value = value.as_ref();
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
    ///
    /// A request has one body: after [`streamed_body`](Self::streamed_body)
    /// this is an error, which `send` returns.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        if self.source.is_some() && self.error.is_none() {
            self.error = Some(Error::request(
                "the request already has a streamed body (RequestBuilder::streamed_body); \
                 a request has one body"
                    .to_owned(),
            ));
        }
        self.body = Some(body.into());
        self
    }

    /// Makes the body one that is written piece by piece, with the
    /// [`RequestBody`] returned: for uploads that should not be held whole,
    /// and for exchanges where requests and responses alternate on one
    /// HTTP/2 stream (gRPC streaming).
    ///
    /// The request is sent by [`send_streaming`](Self::send_streaming) (or
    /// [`send`](Self::send)), and the body is written while that future is
    /// polled or after it returned, so the two run in two tasks, or the
    /// body is written from the task that reads the response:
    ///
    /// ```no_run
    /// use giong_http::client::Client;
    ///
    /// # fn main() -> Result<(), giong_http::client::Error> {
    /// # giong::block_on(async {
    /// let client = Client::new();
    /// let (request, body) = client.post("https://example.com/upload").streamed_body();
    /// let upload = giong::spawn(async move {
    ///     for piece in [&b"first"[..], b"second"] {
    ///         body.send(piece).await?;
    ///     }
    ///     Ok::<(), giong_http::client::Error>(())
    /// });
    /// let response = request.send().await?;
    /// upload.await.expect("the upload task")?;
    /// println!("{}", response.status());
    /// # Ok(())
    /// # })
    /// # }
    /// ```
    ///
    /// Over HTTP/2 the response can arrive while the body is still being
    /// written (full duplex): `send_streaming` returns when the head of the
    /// response does, whether or not the body has ended. Over HTTP/1.1 the
    /// body goes out with `Transfer-Encoding: chunked` and the response is
    /// read once the body has ended. HTTP/3 requests carry bodies held in
    /// memory: a client that learned of HTTP/3 from `Alt-Svc` sends this
    /// request over HTTP/1.1 or HTTP/2, and one with
    /// `ClientBuilder::http3_prior_knowledge` refuses it
    /// ([`ErrorKind::Request`]).
    ///
    /// What differs from a body held in memory, because it cannot be sent
    /// twice: redirects are not followed (the 3xx response is returned),
    /// and a request the server did not process is sent again only if no
    /// piece had been taken yet. Dropping the request, or its response,
    /// makes [`RequestBody::send`] fail.
    ///
    /// A request has one body: after [`body`](Self::body) or
    /// [`json`](Self::json), or a second time, this is an error, which
    /// `send` returns.
    pub fn streamed_body(mut self) -> (Self, RequestBody) {
        if (self.body.is_some() || self.source.is_some()) && self.error.is_none() {
            self.error = Some(Error::request(
                "the request already has a body (RequestBuilder::body, json or \
                 streamed_body); a request has one body"
                    .to_owned(),
            ));
        }
        let (sender, source) = crate::body::channel();
        self.source = Some(source);
        (self, RequestBody { sender })
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
        self.timeout = Some(crate::bounded(timeout));
        self
    }

    /// Sends the request and reads the whole response, following
    /// redirects. A compressed body is decompressed (see
    /// [`ClientBuilder::decompress`]).
    ///
    /// Any status is a response, 404 and 500 included: see
    /// [`Response::error_for_status`]. Dropping the future closes the
    /// connection it was using.
    ///
    /// Not cancel safe: if the future is dropped, the request may already have
    /// reached the server, which may act on it, and the connection it used is
    /// closed.
    pub async fn send(self) -> Result<Response, Error> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let _wait = giong::observe::wait(giong::observe::Wait::Upstream, "http");
        let url = self.url?;
        let client = self.client;
        let timeout = self.timeout.unwrap_or(client.shared.config.timeout);
        let what = url.describe(&self.method);
        let run = async {
            let pending = client
                .run(
                    self.method,
                    url,
                    self.headers,
                    self.body,
                    self.source,
                    false,
                )
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
    ///
    /// Not cancel safe, like [`send`](Self::send).
    pub async fn send_streaming(self) -> Result<StreamingResponse, Error> {
        if let Some(e) = self.error {
            return Err(e);
        }
        // Up to the head; the body is read later, piece by piece.
        let _wait = giong::observe::wait(giong::observe::Wait::Upstream, "http");
        let url = self.url?;
        let client = self.client;
        let timeout = self.timeout.unwrap_or(client.shared.config.timeout);
        let what = url.describe(&self.method);
        let run = client.run(self.method, url, self.headers, self.body, self.source, true);
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

/// Writes a request body piece by piece: the other half of
/// [`RequestBuilder::streamed_body`].
///
/// Dropping it ends the body; [`abort`](Self::abort) cancels the request
/// instead, and [`send_trailers`](Self::send_trailers) ends the body with
/// trailer fields after it.
pub struct RequestBody {
    sender: BodySender,
}

impl RequestBody {
    /// Sends one piece of the body, after waiting until the connection has
    /// taken the previous one: one piece in flight, so a slow server slows
    /// the producer down instead of filling memory. Over HTTP/2 each piece
    /// goes out as DATA frames within the server's flow-control windows.
    /// Empty pieces are skipped.
    ///
    /// Cancel safe: if the future is dropped before it returns, the piece
    /// was not sent.
    ///
    /// # Errors
    ///
    /// An error of kind [`ErrorKind::Closed`] when the body can no longer
    /// be sent: the response ended or failed, the server reset the stream
    /// or closed the connection, or the request or its response was
    /// dropped. Stop producing then; the response (or the error of
    /// `send`) says what happened.
    pub async fn send(&self, piece: impl Into<Vec<u8>>) -> Result<(), Error> {
        self.sender.send(piece).await.map_err(|_| body_closed())
    }

    /// Ends the body, then sends `trailers` after it: in a last HEADERS
    /// frame over HTTP/2, after the last chunk over HTTP/1.1. Empty
    /// trailers end the body as dropping the sender does.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send): the trailers were not sent either.
    pub fn send_trailers(self, trailers: Trailers) -> Result<(), Error> {
        self.sender
            .send_trailers(trailers)
            .map_err(|_| body_closed())
    }

    /// Cancels the request: over HTTP/2 the stream is reset
    /// (`RST_STREAM`, `CANCEL`), over HTTP/1.1 the connection closes
    /// without the end of the body. The server sees an incomplete request,
    /// and the response fails with [`ErrorKind::Closed`].
    pub fn abort(self) {
        self.sender.abort();
    }

    /// `true` once the body can no longer be sent (see [`send`](Self::send)).
    pub fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
}

impl fmt::Debug for RequestBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestBody")
            .field("closed", &self.is_closed())
            .finish()
    }
}

fn body_closed() -> Error {
    Error::new(
        ErrorKind::Closed,
        "the request body can no longer be sent: the response ended or failed, the server \
         reset the stream or closed the connection, or the request was dropped; stop \
         producing the body and read the response or the error of the request"
            .to_owned(),
    )
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
    /// `METHOD url`: the URL is its tail, from `url_at`.
    url: String,
    url_at: usize,
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
                self.url(),
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
                    self.url()
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
        &self.url[self.url_at..]
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("version", &self.version)
            .field("url", &self.url())
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
                self.pending().url(),
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
        self.pending().url()
    }

    /// The trailer fields the server sent after the body: in a last
    /// HEADERS frame over HTTP/2, after the last chunk of a chunked
    /// HTTP/1.1 body.
    ///
    /// `None` until [`chunk`](Self::chunk) has returned `Ok(None)`, and
    /// after that if the server sent none. Fields that mean nothing in
    /// trailers (`content-length`, `set-cookie` and the like) are dropped.
    /// The trailers of one response are limited to 16 KiB over HTTP/1.1
    /// and to [`ClientBuilder::max_head_size`] over HTTP/2. Over HTTP/2
    /// the request said `te: trailers`, for the servers that only send
    /// trailers to a client that reads them.
    pub fn trailers(&self) -> Option<&Trailers> {
        if !self.pending.finished() {
            return None;
        }
        self.pending.body.trailers()
    }

    /// The next piece of the body (decompressed if it was compressed, at
    /// most 64 KiB), or `None` at its end. Waits at most the request's
    /// timeout for it. After an error, the connection is closed and every
    /// further call returns `None`.
    ///
    /// Not cancel safe: if the future is dropped, part of the body may be lost,
    /// so drop the response then.
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
            .field("url", &p.url())
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
            Payload::Whole(Some(b"hi")),
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
        let out = encode_request("GET", &url, &headers, Payload::Whole(None), "", None, false);
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "GET / HTTP/1.1\r\nhost: other\r\n\r\n"
        );
        let out = encode_request("PUT", &url, &[], Payload::Whole(None), "", None, false);
        assert!(String::from_utf8(out)
            .unwrap()
            .ends_with("Content-Length: 0\r\n\r\n"));

        // Through a proxy: the whole URL, and the proxy's credentials.
        let proxy = Proxy::all("http://u:p@proxy:3128").unwrap();
        let url = Url::parse("http://example.com:8080/a?b=1").unwrap();
        let route = proxy.route(&url);
        let out = encode_request(
            "GET",
            &url,
            &[],
            Payload::Whole(None),
            "",
            route.absolute_form(),
            false,
        );
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
