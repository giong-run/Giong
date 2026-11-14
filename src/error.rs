//! Error codes: every error giong raises says what went wrong, how to fix it,
//! and where to read more.
//!
//! Functions keep returning [`io::Error`]. When giong itself raises the error
//! (rather than passing on one from the kernel, such as "connection refused"),
//! its message starts with a stable code and ends with a link to that code's
//! page:
//!
//! ```text
//! GIONG-E0102: no address found for db.internal. Fix: check the spelling of
//! the name, /etc/hosts, and the DNS servers in /etc/resolv.conf. See
//! https://github.com/giong-run/Giong/blob/main/docs/errors/E0102.md
//! ```
//!
//! [`code`] reads the code back, so programs (and the agents that write them)
//! can match on it instead of on the message:
//!
//! ```no_run
//! use giong::error::{self, Code};
//!
//! # async fn f() -> std::io::Result<()> {
//! match giong::net::TcpStream::connect("db.internal:5432").await {
//!     Ok(stream) => { /* ... */ }
//!     Err(e) if error::code(&e) == Some(Code::HostNotFound) => { /* ... */ }
//!     Err(e) => return Err(e),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The companion crates (giong-http, giong-web, giong-tls, giong-postgres,
//! giong-mysql, giong-redis, giong-mcp, giong-compat) use the same codes, so one `match` covers them
//! all. Codes are never reused or renumbered; a code that is no longer raised
//! keeps its page.

use std::borrow::Cow;
use std::error::Error as StdError;
use std::fmt;
use std::io;

/// Where the page for every code lives.
const DOCS: &str = "https://github.com/giong-run/Giong/blob/main/docs/errors";

macro_rules! codes {
    ($( $(#[$doc:meta])* $name:ident = $number:literal, $kind:ident, $summary:literal; )*) => {
        /// A stable code for an error giong raises, shown as `GIONG-E0001`.
        ///
        /// Codes are grouped by where they come from: `E00xx` the runtime,
        /// `E01xx` networking and name resolution, `E02xx` files, `E11xx`
        /// giong-http, `E12xx` giong-web, `E13xx` giong-mcp, `E14xx`
        /// giong-compat, `E2xxx` giong-tls, `E30xx` giong-postgres, `E31xx` giong-mysql and `E32xx`
        /// giong-redis. Each
        /// has a page under `docs/errors/` in the repository;
        /// [`url`](Code::url) links to it.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        #[non_exhaustive]
        #[repr(u16)]
        pub enum Code {
            $( $(#[$doc])* $name = $number, )*
        }

        impl Code {
            /// Every code, in numeric order.
            pub const ALL: &'static [Code] = &[$(Code::$name),*];

            /// The number in the code: 102 for `GIONG-E0102`.
            pub const fn number(self) -> u16 {
                self as u16
            }

            /// The code with this number, if there is one.
            pub const fn from_number(number: u16) -> Option<Code> {
                match number {
                    $( $number => Some(Code::$name), )*
                    _ => None,
                }
            }

            /// What the code means, in a few words: the title of its page.
            pub const fn summary(self) -> &'static str {
                match self {
                    $( Code::$name => $summary, )*
                }
            }

            /// The [`io::ErrorKind`] an [`Error`] with this code gets when it
            /// is turned into an [`io::Error`].
            pub const fn kind(self) -> io::ErrorKind {
                match self {
                    $( Code::$name => io::ErrorKind::$kind, )*
                }
            }
        }
    };
}

codes! {
    // ------------------------------------------------------------ runtime
    /// `io_uring_setup` failed: io_uring is blocked by a seccomp profile (as
    /// in Docker's default one) or the `kernel.io_uring_disabled` sysctl, or
    /// missing, as in gVisor. The kernel's error kind (`PermissionDenied` or
    /// `Unsupported`) is kept.
    IoUringUnavailable = 1, Unsupported, "io_uring is not available";
    /// The kernel lacks a feature giong needs, such as buffer rings (Linux
    /// 6.0). The kernel's error kind is kept.
    KernelTooOld = 2, Unsupported, "the kernel is too old";
    /// Another runtime in this process already handles `SIGINT` and
    /// `SIGTERM`.
    SignalsTaken = 3, AlreadyExists, "another runtime handles the signals";
    /// A [`Builder`](crate::Builder) setting is out of range.
    InvalidConfig = 4, InvalidInput, "invalid runtime setting";
    /// The kernel returned something giong does not expect. A bug in giong
    /// or in the kernel.
    Unexpected = 5, Other, "unexpected result from the kernel";
    /// A one-off read was used on a stream or socket that already has a
    /// multishot `recv` running.
    MixedReads = 6, InvalidInput, "read used after recv";
    /// [`time::timeout`](crate::time::timeout) ran out of time.
    TimedOut = 7, TimedOut, "the time limit passed";
    /// [`Handle::spawn_on`](crate::Handle::spawn_on) was given a worker index
    /// the runtime does not have. It panics with this code, since the index
    /// is a bug in the caller.
    NoSuchWorker = 8, InvalidInput, "no such worker";
    /// A kernel from 6.0 on refused to register an io_uring buffer ring,
    /// even with giong's workarounds for the kernel bugs it knows. The
    /// kernel's error kind is kept.
    BufferRingRefused = 9, Unsupported, "the kernel refused io_uring buffer rings";
    /// The kernel would not give io_uring memory because the user's
    /// locked-memory limit (`RLIMIT_MEMLOCK`) is used up: Linux 6.14 to 7.2
    /// count every ring against it, for all of the user's processes
    /// together, unless the process has `CAP_IPC_LOCK`. The kernel's error
    /// kind (`OutOfMemory`) is kept.
    LockedMemoryLimit = 10, OutOfMemory, "the locked-memory limit is used up";

    // ------------------------------------------------- network and names
    /// A `"host:port"` string is not one.
    InvalidAddress = 100, InvalidInput, "invalid address";
    /// A host name has characters or labels DNS does not allow.
    InvalidHostName = 101, InvalidInput, "invalid host name";
    /// The name has no address: not in `/etc/hosts`, and DNS says it does not
    /// exist or has no `A` or `AAAA` record.
    HostNotFound = 102, NotFound, "host not found";
    /// A list of addresses to connect or bind to was empty.
    NoAddresses = 103, InvalidInput, "no address to use";
    /// There is no DNS server to ask:
    /// [`ResolverConfig::nameservers`](crate::net::ResolverConfig::nameservers)
    /// was given none. (Without a `nameserver` line, `/etc/resolv.conf`
    /// means `127.0.0.1:53`, as in glibc.)
    NoDnsServer = 104, NotFound, "no DNS server configured";
    /// No DNS server answered in time.
    DnsTimeout = 105, TimedOut, "DNS server did not answer";
    /// A DNS server answered with an error, such as `SERVFAIL` or `REFUSED`.
    DnsServerFailure = 106, Other, "DNS server failed";
    /// A DNS server sent an answer that cannot be read.
    DnsBadResponse = 107, InvalidData, "malformed DNS answer";
    /// The query could not even reach the DNS server: nothing listens at
    /// its address (connection refused), or the network says the server is
    /// unreachable. The kernel's error is the `source()`.
    DnsServerUnreachable = 108, Other, "cannot reach the DNS server";
    /// [`UdpSocket::read_from`](crate::net::UdpSocket::read_from) was given
    /// a buffer with no capacity, into which a datagram would be received
    /// and lost.
    EmptyBuffer = 109, InvalidInput, "buffer with no room";

    // -------------------------------------------------------------- files
    /// A path contains a NUL byte, which Linux paths cannot.
    InvalidPath = 200, InvalidInput, "invalid path";
    /// [`TcpStream::send_file`](crate::net::TcpStream::send_file) was asked
    /// for bytes past the end of the file. What the file had was sent.
    FileTooShort = 201, UnexpectedEof, "the file ended before the bytes to send";
    /// A file offset above `i64::MAX`, the largest Linux has, was passed to
    /// [`File::read_at`](crate::fs::File::read_at) or
    /// [`File::write_at`](crate::fs::File::write_at) and the other methods
    /// that take one, or a range that ends above it to
    /// [`File::allocate`](crate::fs::File::allocate).
    OffsetOutOfRange = 202, InvalidInput, "file offset out of range";
    /// [`OpenOptions`](crate::fs::OpenOptions) were given a combination
    /// `open(2)` cannot do: no access mode, `truncate` or `create` without
    /// `write`, or `append` with `truncate`. Nothing was opened.
    InvalidOpenOptions = 203, InvalidInput, "invalid combination of open options";

    // ------------------------------------------------- giong-http client
    /// The URL, a header, the method or the client's settings are not
    /// valid; nothing was sent.
    HttpClientRequest = 1101, InvalidInput, "invalid HTTP request";
    /// Opening a connection failed: name resolution, TCP, TLS, or the
    /// connect timeout.
    HttpClientConnect = 1102, Other, "cannot connect to the HTTP server";
    /// The request did not finish within its timeout.
    HttpClientTimeout = 1103, TimedOut, "HTTP request timed out";
    /// The response was larger than the client's limit.
    HttpClientTooLarge = 1104, Other, "HTTP response too large";
    /// A redirect was not followed.
    HttpClientRedirect = 1105, Other, "redirect not followed";
    /// The connection closed before the whole response arrived.
    HttpClientClosed = 1106, ConnectionAborted, "connection closed before the response";
    /// The server sent something that is not a valid HTTP/1.1 response.
    HttpClientProtocol = 1107, InvalidData, "malformed HTTP response";
    /// The response has a 4xx or 5xx status.
    HttpClientStatus = 1108, Other, "HTTP error status";
    /// The response body could not be read as asked.
    HttpClientDecode = 1109, InvalidData, "cannot decode the response body";
    /// The proxy refused the request or the tunnel, or did not answer as a
    /// proxy does.
    HttpClientProxy = 1110, Other, "the HTTP proxy refused the request";

    // ------------------------------------------------- giong-http server
    /// A request body read as a stream went past the limit its handler set
    /// with `Handler::stream_body`.
    HttpBodyTooLarge = 1111, InvalidData, "request body too large";
    /// A request body read as a stream ended early: the client closed the
    /// connection or sent nothing for the request timeout, or the handler
    /// answered before reading all of it.
    HttpBodyIncomplete = 1112, UnexpectedEof, "request body cut short";
    /// A request body read as a stream has broken chunked framing.
    HttpBodyMalformed = 1113, InvalidData, "malformed request body";
    /// A streamed response body cannot be sent any more: the client went
    /// away, or the response is to a `HEAD` request or has a status without
    /// a body.
    HttpBodyNotSent = 1114, BrokenPipe, "response body not sent";
    /// The other end broke the HTTP/2 protocol. The server closes the
    /// connection with a GOAWAY frame whose debug data carries this code
    /// and the rule that was broken.
    Http2Protocol = 1115, InvalidData, "HTTP/2 protocol error";
    /// An HTTP/2 client cancelled (or had refused) more streams than the
    /// server allows in a short time, the pattern of the "rapid reset"
    /// attack. The server closes the connection with `ENHANCE_YOUR_CALM`.
    Http2TooManyResets = 1116, ConnectionAborted, "too many HTTP/2 streams reset";
    /// An HTTP/2 client sent more frames that do no work (PING, SETTINGS,
    /// PRIORITY, empty DATA or CONTINUATION frames, WINDOW_UPDATE frames
    /// with nothing to send, frames for closed streams, unknown frame
    /// types) than the server allows in a short time. The server closes
    /// the connection with `ENHANCE_YOUR_CALM`.
    Http2Flood = 1117, ConnectionAborted, "too many HTTP/2 control frames";
    /// An HTTP/2 header block (HEADERS and its CONTINUATION frames) grew
    /// past the limit before it ended. The server closes the connection
    /// with `ENHANCE_YOUR_CALM`.
    Http2HeadersTooLarge = 1118, InvalidData, "HTTP/2 header block too large";

    // --------------------------------------------------------- giong-web
    /// A route pattern given to the router is not valid. It panics with
    /// this code when the route is added, since the pattern is a bug in the
    /// program.
    WebInvalidRoute = 1201, InvalidInput, "invalid route pattern";
    /// Two routes have the same method and the same pattern (parameter
    /// names aside). It panics with this code when the second one is added.
    WebDuplicateRoute = 1202, AlreadyExists, "route added twice";
    /// No route matches the request's path: answered with `404 Not Found`.
    WebNoRoute = 1203, NotFound, "no route for the path";
    /// Routes match the path, but none for the request's method: answered
    /// with `405 Method Not Allowed` and an `Allow` header.
    WebMethodNotAllowed = 1204, Unsupported, "method not allowed";
    /// A path or query parameter is missing or cannot be read as the type
    /// the handler asked for: answered with `400 Bad Request`.
    WebBadParameter = 1205, InvalidInput, "invalid path or query parameter";
    /// A handler asked for a path parameter its route does not have: a bug
    /// in the program, answered with `500 Internal Server Error`.
    WebNoSuchParameter = 1206, InvalidInput, "the route has no such parameter";
    /// The request body cannot be read as the handler asked, such as JSON
    /// that does not parse (`400`) or does not fit the expected type
    /// (`422`).
    WebBadBody = 1207, InvalidData, "unreadable request body";
    /// The request body has a `Content-Type` the handler does not take:
    /// answered with `415 Unsupported Media Type`.
    WebUnsupportedMediaType = 1208, InvalidInput, "unsupported Content-Type";
    /// The handler did not answer within the `Timeout` middleware's limit:
    /// answered with `503 Service Unavailable`.
    WebTimeout = 1209, TimedOut, "no response in time";
    /// The handler failed: answered with `500 Internal Server Error`, the
    /// cause written to the log, never sent.
    WebInternal = 1210, Other, "the handler failed";
    /// A service the handler needs (a database, a cache, another HTTP API)
    /// cannot be reached or is overloaded: answered with `503 Service
    /// Unavailable`.
    WebUnavailable = 1211, Other, "a service the handler needs is unavailable";
    /// A CORS preflight request asked for an origin, method or header the
    /// `Cors` middleware does not allow: answered with `403 Forbidden`.
    WebCorsRejected = 1212, PermissionDenied, "cross-origin request refused";
    /// A giong-web middleware was given settings that cannot work together.
    /// It panics with this code when the middleware is built.
    WebConfig = 1213, InvalidInput, "invalid giong-web setting";

    // --------------------------------------------------------- giong-mcp
    /// The MCP server was asked to listen on an address other machines can
    /// reach. It only listens on loopback addresses or a Unix socket.
    McpNotLocal = 1301, PermissionDenied, "the MCP server listens on this machine only";
    /// The address given to the MCP server, or `GIONG_MCP`, is not one.
    McpBadAddress = 1302, InvalidInput, "invalid address for the MCP server";

    // ------------------------------------------------------ giong-compat
    /// A write that giong-compat's `Compat` had buffered failed earlier, so
    /// the stream cannot be written any more. The first call after the
    /// failure returned the kernel's own error; later ones return this.
    CompatWriteFailed = 1401, BrokenPipe, "an earlier buffered write failed";
    /// A write was attempted on a `Compat` stream after its sending
    /// direction was shut down (`poll_shutdown`, `poll_close`).
    CompatShutDown = 1402, BrokenPipe, "write after shutdown";
    /// A `Compat` setting is out of range. It panics with this code, since
    /// the value is a bug in the program.
    CompatConfig = 1403, InvalidInput, "invalid giong-compat setting";

    // --------------------------------------------------------- giong-tls
    /// No trusted certificate authority: none installed on the machine, or
    /// none given.
    NoTrustedRoots = 2001, NotFound, "no trusted certificate authorities";
    /// A certificate file or PEM data cannot be read or holds no
    /// certificate.
    BadCertificateData = 2002, InvalidData, "unreadable certificate data";
    /// A TLS server name is neither a DNS name nor an IP address.
    InvalidServerName = 2003, InvalidInput, "invalid TLS server name";
    /// The peer's certificate was rejected: expired, issued by an authority
    /// that is not trusted, or not valid for the name.
    TlsCertificate = 2004, InvalidData, "certificate rejected";
    /// The peer broke the TLS protocol or sent an alert.
    TlsProtocol = 2005, InvalidData, "TLS protocol error";
    /// The TLS handshake did not finish in time.
    TlsHandshakeTimeout = 2006, TimedOut, "TLS handshake timed out";
    /// The peer closed the connection during the TLS handshake.
    TlsHandshakeEof = 2007, UnexpectedEof, "connection closed during the TLS handshake";
    /// A TLS write failed or was cancelled, so the stream cannot send any
    /// more.
    TlsBroken = 2008, BrokenPipe, "TLS stream broken";
    /// Kernel TLS was required (`Ktls::Required`) and cannot be used: the
    /// kernel has no `tls` module, does not support a cipher suite the
    /// configuration allows, the socket refused it, or `TcpStream::recv` was
    /// already used on the stream.
    KtlsUnavailable = 2009, Unsupported, "kernel TLS is not available";
    /// The kernel refused a connection's TLS keys: after the handshake (the
    /// connection was closed), or new keys for a key update (nothing more
    /// can be sent).
    KtlsSetup = 2010, Other, "the kernel refused the TLS keys";
    /// The peer changed its TLS 1.3 keys and the kernel that encrypts the
    /// connection cannot take new keys (Linux before 6.14).
    KtlsKeyUpdate = 2011, Unsupported, "the kernel cannot change TLS keys";
    /// The connection has sent as many TLS records as its cipher allows under
    /// one key and cannot change keys (TLS 1.2, or TLS 1.3 on a kernel older
    /// than 6.14), so it sent `close_notify`.
    TlsKeysExhausted = 2012, ConnectionAborted, "TLS keys used up";

    // ---------------------------------------------------- giong-postgres
    /// The configuration or connection URL is not valid.
    PostgresConfig = 3001, InvalidInput, "invalid PostgreSQL configuration";
    /// Opening a connection failed: name resolution, TCP, TLS, or the
    /// connect timeout.
    PostgresConnect = 3002, Other, "cannot connect to PostgreSQL";
    /// The server refused the credentials, or the authentication method is
    /// not supported.
    PostgresAuth = 3003, PermissionDenied, "PostgreSQL authentication failed";
    /// The server reported an error for a statement; its SQLSTATE says which.
    PostgresServer = 3004, Other, "PostgreSQL server error";
    /// A value could not be converted between Rust and PostgreSQL.
    PostgresType = 3005, InvalidData, "PostgreSQL type mismatch";
    /// `query_one` found no row, or more than one.
    PostgresRowCount = 3006, Other, "wrong number of rows";
    /// A result was larger than the configured maximum.
    PostgresTooLarge = 3007, Other, "PostgreSQL result too large";
    /// The connection is closed.
    PostgresClosed = 3008, ConnectionAborted, "PostgreSQL connection closed";
    /// No pooled connection became free in time.
    PostgresPoolTimeout = 3009, TimedOut, "PostgreSQL pool wait timed out";
    /// The server sent something that is not the PostgreSQL protocol.
    PostgresProtocol = 3010, InvalidData, "PostgreSQL protocol error";
    /// The statement cannot be sent this way, such as `BEGIN` on the
    /// connections a pool shares between tasks; nothing was sent.
    PostgresStatement = 3011, InvalidInput, "statement refused before sending";

    // ------------------------------------------------------- giong-mysql
    /// The configuration or connection URL is not valid.
    MysqlConfig = 3101, InvalidInput, "invalid MySQL configuration";
    /// Opening a connection failed: name resolution, TCP, TLS, or the
    /// connect timeout.
    MysqlConnect = 3102, Other, "cannot connect to MySQL";
    /// The server refused the credentials, or the authentication method is
    /// not supported or not safe on this connection.
    MysqlAuth = 3103, PermissionDenied, "MySQL authentication failed";
    /// The server reported an error for a statement; its number says which.
    MysqlServer = 3104, Other, "MySQL server error";
    /// A value could not be converted between Rust and MySQL.
    MysqlType = 3105, InvalidData, "MySQL type mismatch";
    /// `query_one` found no row or more than one, or `insert` found no
    /// generated id.
    MysqlRowCount = 3106, Other, "wrong number of rows";
    /// A result was larger than the configured maximum.
    MysqlTooLarge = 3107, Other, "MySQL result too large";
    /// The connection is closed.
    MysqlClosed = 3108, ConnectionAborted, "MySQL connection closed";
    /// No pooled connection became free in time.
    MysqlPoolTimeout = 3109, TimedOut, "MySQL pool wait timed out";
    /// The server sent something that is not the MySQL protocol.
    MysqlProtocol = 3110, InvalidData, "MySQL protocol error";
    /// The statement cannot be sent this way, such as `START TRANSACTION`
    /// on the connections a pool shares between tasks; nothing was sent.
    MysqlStatement = 3111, InvalidInput, "statement refused before sending";

    // ------------------------------------------------------- giong-redis
    /// The configuration or connection URL is not valid.
    RedisConfig = 3201, InvalidInput, "invalid Redis configuration";
    /// Opening a connection failed: name resolution, TCP, TLS, the
    /// handshake, or the connect timeout.
    RedisConnect = 3202, Other, "cannot connect to Redis";
    /// The server refused the user name or password.
    RedisAuth = 3203, PermissionDenied, "Redis authentication failed";
    /// The server answered a command with an error, such as `WRONGTYPE`.
    RedisServer = 3204, Other, "Redis server error";
    /// A reply could not be converted to the Rust type asked for.
    RedisType = 3205, InvalidData, "Redis type mismatch";
    /// The command cannot be sent this way, such as `MULTI` or a blocking
    /// command on a connection other tasks share; nothing was sent.
    RedisCommand = 3206, InvalidInput, "command refused before sending";
    /// A reply was larger than the configured maximum.
    RedisTooLarge = 3207, Other, "Redis reply too large";
    /// The connection is closed.
    RedisClosed = 3208, ConnectionAborted, "Redis connection closed";
    /// No pooled connection became free in time.
    RedisPoolTimeout = 3209, TimedOut, "Redis pool wait timed out";
    /// The server sent something that is not RESP.
    RedisProtocol = 3210, InvalidData, "Redis protocol error";
    /// The reply did not arrive within the response timeout.
    RedisTimeout = 3211, TimedOut, "Redis reply timed out";
}

impl Code {
    /// The page about this code: what it means, why it happens and how to
    /// fix it.
    pub fn url(self) -> String {
        format!("{DOCS}/E{:04}.md", self.number())
    }
}

impl fmt::Display for Code {
    /// `GIONG-E0102`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GIONG-E{:04}", self.number())
    }
}

/// An error giong raised, with its [`Code`], what went wrong, and how to fix
/// it.
///
/// It travels inside an [`io::Error`] (`From<Error> for io::Error` gives it
/// the code's [`kind`](Code::kind)); [`code`] finds it again. Shown as
/// `GIONG-E0102: <what went wrong>. Fix: <how to fix it> See <url>`.
pub struct Error {
    code: Code,
    message: Cow<'static, str>,
    fix: Cow<'static, str>,
    source: Option<Box<dyn StdError + Send + Sync>>,
    /// `source` is a companion crate's error that formats itself, code
    /// included; see [`Error::wrap`].
    wrapped: bool,
}

impl Error {
    /// An error with `code`. `message` says what went wrong, without a
    /// final full stop; `fix` says what to do about it, in one or two
    /// sentences.
    pub fn new(
        code: Code,
        message: impl Into<Cow<'static, str>>,
        fix: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            fix: fix.into(),
            source: None,
            wrapped: false,
        }
    }

    /// The same error, caused by `source` (often the kernel's error, so its
    /// `raw_os_error` stays reachable through [`std::error::Error::source`]).
    pub fn with_source(mut self, source: impl Into<Box<dyn StdError + Send + Sync>>) -> Self {
        self.source = Some(source.into());
        self.wrapped = false;
        self
    }

    /// Carries a companion crate's own error type, which already shows the
    /// code, what went wrong and the link, under `code`. It is shown exactly
    /// as `error` shows itself, and [`get_ref`](Self::get_ref) returns it.
    pub fn wrap(code: Code, error: impl StdError + Send + Sync + 'static) -> Self {
        Self {
            code,
            message: Cow::Borrowed(""),
            fix: Cow::Borrowed(""),
            source: Some(Box::new(error)),
            wrapped: true,
        }
    }

    /// The code.
    pub fn code(&self) -> Code {
        self.code
    }

    /// What went wrong.
    pub fn message(&self) -> Cow<'_, str> {
        match (&self.source, self.wrapped) {
            (Some(inner), true) => Cow::Owned(inner.to_string()),
            _ => Cow::Borrowed(&self.message),
        }
    }

    /// How to fix it; empty for a [wrapped](Self::wrap) error, whose
    /// message says it.
    pub fn fix(&self) -> &str {
        &self.fix
    }

    /// The error passed to [`wrap`](Self::wrap), for downcasting to the
    /// companion crate's type.
    pub fn get_ref(&self) -> Option<&(dyn StdError + Send + Sync + 'static)> {
        match (&self.source, self.wrapped) {
            (Some(inner), true) => Some(inner.as_ref()),
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let (Some(inner), true) = (&self.source, self.wrapped) {
            return fmt::Display::fmt(inner, f);
        }
        write!(f, "{}: {}.", self.code, self.message)?;
        if !self.fix.is_empty() {
            write!(f, " Fix: {}", self.fix)?;
        }
        write!(f, " See {}", self.code.url())
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Error")
            .field("code", &self.code)
            .field("message", &self.message())
            .field("fix", &self.fix)
            .field("source", &self.source)
            .finish()
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match (&self.source, self.wrapped) {
            // The wrapped error is this error, shown the same way; its
            // cause is the next one down.
            (Some(inner), true) => inner.source(),
            (Some(source), false) => Some(source.as_ref()),
            (None, _) => None,
        }
    }
}

impl From<Error> for io::Error {
    fn from(e: Error) -> Self {
        io::Error::new(e.code.kind(), e)
    }
}

/// An [`io::Error`] carrying a new [`Error`], for giong's own errors.
pub(crate) fn raise(
    code: Code,
    message: impl Into<Cow<'static, str>>,
    fix: impl Into<Cow<'static, str>>,
) -> io::Error {
    Error::new(code, message, fix).into()
}

/// The code of an error giong raised, or `None` for one it passed on (from
/// the kernel, say) or that came from elsewhere.
///
/// Looks at `err` and then down its chain of
/// [`source`](std::error::Error::source)s, so it also finds the code when
/// another error wraps giong's.
pub fn code(err: &io::Error) -> Option<Code> {
    let mut next: Option<&(dyn StdError + 'static)> = Some(err.get_ref()?);
    while let Some(e) = next {
        if let Some(e) = e.downcast_ref::<Error>() {
            return Some(e.code);
        }
        if let Some(e) = e.downcast_ref::<io::Error>() {
            next = e.get_ref().map(|inner| inner as &(dyn StdError + 'static));
            continue;
        }
        next = e.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_unique_and_round_trip() {
        let mut seen = std::collections::HashSet::new();
        for &code in Code::ALL {
            assert!(seen.insert(code.number()), "{code} is used twice");
            assert_eq!(Code::from_number(code.number()), Some(code));
            assert!(!code.summary().is_empty());
        }
        assert!(Code::ALL.windows(2).all(|w| w[0] < w[1]), "keep ALL sorted");
        assert_eq!(Code::from_number(0), None);
    }

    #[test]
    fn shows_code_message_fix_and_link() {
        let e = Error::new(
            Code::HostNotFound,
            "no address found for db.internal",
            "check the spelling.",
        );
        assert_eq!(
            e.to_string(),
            "GIONG-E0102: no address found for db.internal. Fix: check the spelling. See \
             https://github.com/giong-run/Giong/blob/main/docs/errors/E0102.md"
        );
        let io: io::Error = e.into();
        assert_eq!(io.kind(), io::ErrorKind::NotFound);
        assert_eq!(code(&io), Some(Code::HostNotFound));
    }

    #[test]
    fn finds_the_code_down_the_chain() {
        let inner: io::Error = Error::new(Code::DnsTimeout, "no answer", "retry.").into();
        let outer = io::Error::other(inner);
        assert_eq!(code(&outer), Some(Code::DnsTimeout));
        assert_eq!(code(&io::Error::from(io::ErrorKind::NotFound)), None);
        assert_eq!(code(&io::Error::other("plain")), None);
    }

    #[derive(Debug)]
    struct Companion;
    impl fmt::Display for Companion {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("GIONG-E3003: refused")
        }
    }
    impl StdError for Companion {}

    #[test]
    fn a_wrapped_error_shows_itself() {
        let e = Error::wrap(Code::PostgresAuth, Companion);
        assert_eq!(e.to_string(), "GIONG-E3003: refused");
        assert!(e.get_ref().unwrap().downcast_ref::<Companion>().is_some());
        assert!(e.source().is_none());
        let io = io::Error::new(io::ErrorKind::PermissionDenied, e);
        assert_eq!(code(&io), Some(Code::PostgresAuth));
    }

    #[test]
    fn the_source_stays_reachable() {
        let os = io::Error::from_raw_os_error(libc::EPERM);
        let e = Error::new(Code::IoUringUnavailable, "blocked", "allow it.").with_source(os);
        let source = e.source().unwrap().downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.raw_os_error(), Some(libc::EPERM));
    }
}
