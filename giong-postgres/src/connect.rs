//! Opening a connection: the socket, TLS, the startup message and
//! authentication.

use std::io;
use std::net::Shutdown;
use std::ops::{Deref, Range};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::time::Duration;

use giong::net::{TcpStream, UnixStream};
use giong::RecvBuf;
use giong_tls::TlsStream;
use socket2::{SockRef, TcpKeepalive};

use crate::auth::{md5_password, Scram};
use crate::config::{AuthMethod, Config, Host};
use crate::error::{DbError, Error, ErrorKind};
use crate::protocol::{self, frame, parse_backend, Backend};

/// A dead peer is noticed after about a minute: keepalive probes start after
/// 30 s of silence and give up after three more 10 s apart, and data the
/// server has not acknowledged for 60 s fails the connection.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_RETRIES: u32 = 3;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const USER_TIMEOUT: Duration = Duration::from_secs(60);

/// Free room asked of the read buffer before each receive: replies that
/// arrive together are taken in one receive up to this much.
const READ_ROOM: usize = 64 * 1024;

/// The socket under a connection.
pub(crate) enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
    Tls(Box<TlsStream>),
}

/// Received bytes, from whichever socket.
pub(crate) enum Chunk {
    Pooled(RecvBuf),
    Owned(Vec<u8>),
}

impl Deref for Chunk {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Chunk::Pooled(b) => b,
            Chunk::Owned(v) => v,
        }
    }
}

impl Stream {
    /// The next bytes; empty at the end of the stream. Cancel safe.
    pub(crate) async fn recv(&self) -> io::Result<Chunk> {
        match self {
            Stream::Tcp(s) => s.recv().await.map(Chunk::Pooled),
            Stream::Unix(s) => s.recv().await.map(Chunk::Pooled),
            Stream::Tls(s) => s.recv().await.map(|r| Chunk::Owned(r.into_vec())),
        }
    }

    /// Receives more bytes into `buf`; `Ok(0)` at the end of the stream.
    ///
    /// On plain TCP and Unix sockets the kernel writes straight into `buf`'s
    /// storage (a one-off receive), which saves copying every byte out of a
    /// pooled buffer. Not cancel safe there: dropping the future loses the
    /// bytes in flight and leaves `buf` empty, so it is only for a loop that
    /// owns the connection, and drops `buf` along with the future.
    pub(crate) async fn read_into(&self, buf: &mut ReadBuf) -> io::Result<usize> {
        let tail = match self {
            Stream::Tcp(_) | Stream::Unix(_) => buf.lend(READ_ROOM),
            Stream::Tls(_) => {
                let chunk = self.recv().await?;
                buf.extend(&chunk);
                return Ok(chunk.len());
            }
        };
        let (result, tail) = match self {
            Stream::Tcp(s) => s.read(tail).await,
            Stream::Unix(s) => s.read(tail).await,
            Stream::Tls(_) => unreachable!("only sockets read in place"),
        };
        let n = giong::IoBuf::len(&tail);
        buf.filled(tail);
        result.map(|_| n)
    }

    pub(crate) async fn write_all(&self, buf: Vec<u8>) -> (io::Result<()>, Vec<u8>) {
        match self {
            Stream::Tcp(s) => s.write_all(buf).await,
            Stream::Unix(s) => s.write_all(buf).await,
            Stream::Tls(s) => s.write_all(buf).await,
        }
    }

    /// Shuts the socket down in `how`, which also wakes a task waiting in
    /// `recv`.
    pub(crate) fn shutdown(&self, how: Shutdown) {
        let _ = match self {
            Stream::Tcp(s) => s.shutdown(how),
            Stream::Unix(s) => s.shutdown(how),
            Stream::Tls(s) => s.tcp().shutdown(how),
        };
    }
}

/// The free end of a [`ReadBuf`]'s storage, lent to a one-off receive: the
/// kernel writes after the bytes already there.
pub(crate) struct Tail {
    data: Vec<u8>,
    /// Where the free end starts: the length when lent.
    base: usize,
}

// SAFETY: the bytes are a `Vec`'s, on the heap, so they do not move when the
// value moves, and only the owner of the value can change them. `as_ptr` and
// `len` cover the bytes written after `base`.
unsafe impl giong::IoBuf for Tail {
    fn as_ptr(&self) -> *const u8 {
        self.data[self.base..].as_ptr()
    }
    fn len(&self) -> usize {
        self.data.len() - self.base
    }
}

// SAFETY: `as_mut_ptr` points at `base` in an allocation of
// `data.capacity()` bytes, so `capacity() = data.capacity() - base` bytes are
// writable from it; `set_init` marks only bytes the kernel wrote.
unsafe impl giong::IoBufMut for Tail {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        // The inherent method, valid for the whole allocation.
        self.data.as_mut_ptr().wrapping_add(self.base)
    }
    fn capacity(&self) -> usize {
        self.data.capacity() - self.base
    }
    unsafe fn set_init(&mut self, n: usize) {
        // SAFETY: the caller promises the `n` bytes after `base` were
        // written, and they fit in the allocation (`n <= capacity()`).
        unsafe { self.data.set_len(self.base + n) }
    }
}

/// Bytes received and not yet taken apart into messages.
pub(crate) struct ReadBuf {
    data: Vec<u8>,
    start: usize,
    /// Bytes of an oversized message still to be thrown away as they arrive.
    skip: usize,
}

impl ReadBuf {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::with_capacity(8192),
            start: 0,
            skip: 0,
        }
    }

    pub(crate) fn extend(&mut self, mut chunk: &[u8]) {
        if self.skip > 0 {
            let n = self.skip.min(chunk.len());
            self.skip -= n;
            chunk = &chunk[n..];
        }
        self.make_room();
        self.data.extend_from_slice(chunk);
    }

    /// Lends the free end of the storage, with room for at least `want`
    /// bytes, to a receive that writes straight into it (no copy through a
    /// pooled buffer). Give it back with [`filled`](Self::filled).
    pub(crate) fn lend(&mut self, want: usize) -> Tail {
        self.make_room();
        if self.data.capacity() - self.data.len() < want {
            self.data.reserve(want);
        }
        let data = std::mem::take(&mut self.data);
        Tail {
            base: data.len(),
            data,
        }
    }

    /// Takes back the storage lent by [`lend`](Self::lend), with the bytes
    /// the receive wrote after what was there.
    pub(crate) fn filled(&mut self, tail: Tail) {
        let Tail { mut data, base } = tail;
        if self.skip > 0 {
            let n = self.skip.min(data.len() - base);
            self.skip -= n;
            data.drain(base..base + n);
        }
        self.data = data;
    }

    /// Drops the bytes already taken apart, when that is cheap.
    fn make_room(&mut self) {
        if self.start == self.data.len() {
            self.data.clear();
            self.start = 0;
        } else if self.start > 0 && self.start >= self.data.len() / 2 {
            self.data.drain(..self.start);
            self.start = 0;
        }
    }

    /// The next message's tag and total length, once its header is here.
    pub(crate) fn peek(&self) -> Result<Option<(u8, usize)>, Error> {
        frame(&self.data[self.start..])
    }

    /// The body of the message of total length `len` at the front, if all of
    /// it is here.
    pub(crate) fn body(&self, len: usize) -> Option<Range<usize>> {
        (self.data.len() - self.start >= len).then(|| self.start + 5..self.start + len)
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> &[u8] {
        &self.data[range]
    }

    pub(crate) fn consume(&mut self, len: usize) {
        self.start += len;
    }

    /// Throws away the message of total length `len` at the front, including
    /// the part that has not arrived yet.
    pub(crate) fn skip(&mut self, len: usize) {
        let here = (self.data.len() - self.start).min(len);
        self.start += here;
        self.skip = len - here;
    }
}

/// What the server said while the connection was being set up.
pub(crate) struct Startup {
    pub stream: Stream,
    pub buf: ReadBuf,
    pub params: Vec<(String, String)>,
    pub key: Option<(i32, i32)>,
    pub status: u8,
}

fn connect_error(config: &Config, message: impl std::fmt::Display, e: io::Error) -> Error {
    Error::io(
        ErrorKind::Connect,
        format!("cannot connect to {}: {message}", config.target()),
        e,
    )
}

/// Opens the socket and, when the configuration asks for it, starts TLS.
pub(crate) async fn open(config: &Config) -> Result<Stream, Error> {
    match &config.host {
        Host::Unix(dir) => {
            let path = config.socket_path(dir);
            let stream = UnixStream::connect(&path).await.map_err(|e| {
                connect_error(
                    config,
                    "is the server running, and is this its socket directory?",
                    e,
                )
            })?;
            Ok(Stream::Unix(stream))
        }
        Host::Tcp(host) => {
            let tls = config.tls_connector()?;
            let tcp = TcpStream::connect((host.as_str(), config.port))
                .await
                .map_err(|e| connect_error(config, "is the server running and reachable?", e))?;
            tune(&tcp).map_err(|e| connect_error(config, "cannot set socket options", e))?;
            let Some(connector) = tls else {
                return Ok(Stream::Tcp(tcp));
            };
            let mut request = Vec::with_capacity(8);
            protocol::ssl_request(&mut request);
            let (sent, _) = tcp.write_all(request).await;
            sent.map_err(|e| connect_error(config, "sending the TLS request failed", e))?;
            let answer = tcp.recv().await.map_err(|e| {
                connect_error(
                    config,
                    "waiting for the answer to the TLS request failed",
                    e,
                )
            })?;
            match &answer[..] {
                b"S" => {}
                b"N" => {
                    return Err(Error::new(
                        ErrorKind::Connect,
                        format!(
                            "the server at {} does not accept TLS (ssl is off in its \
                             postgresql.conf); turn TLS on there, or add sslmode=disable to the \
                             URL if the network is trusted",
                            config.target()
                        ),
                    ))
                }
                [] => {
                    return Err(Error::new(
                        ErrorKind::Connect,
                        format!(
                            "the server at {} closed the connection when asked for TLS",
                            config.target()
                        ),
                    ))
                }
                // A server must answer with exactly one byte: anything after
                // it would be read as if it had come through TLS
                // (CVE-2021-23222).
                _ => {
                    return Err(Error::protocol(format!(
                        "the server at {} answered the TLS request with {} bytes instead of one",
                        config.target(),
                        answer.len()
                    )))
                }
            }
            drop(answer);
            let tls = connector.connect(host, tcp).await.map_err(|e| {
                connect_error(config, format!("the TLS handshake with {host} failed"), e)
            })?;
            Ok(Stream::Tls(Box::new(tls)))
        }
    }
}

/// Turns off Nagle's algorithm (requests are written whole) and sets up
/// keepalive (and, on Linux, `TCP_USER_TIMEOUT`), so that a server that
/// vanished is noticed.
fn tune(tcp: &TcpStream) -> io::Result<()> {
    tcp.set_nodelay(true)?;
    // SAFETY: the descriptor belongs to `tcp`, which outlives this borrow.
    let fd = unsafe { BorrowedFd::borrow_raw(tcp.as_raw_fd()) };
    let sock = SockRef::from(&fd);
    sock.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_RETRIES),
    )?;
    #[cfg(target_os = "linux")]
    sock.set_tcp_user_timeout(Some(USER_TIMEOUT))?;
    Ok(())
}

/// Reads until a whole message is buffered; returns its tag and body range.
async fn next_message(stream: &Stream, buf: &mut ReadBuf) -> Result<(u8, Range<usize>), Error> {
    loop {
        if let Some((tag, len)) = buf.peek()? {
            if len > 1 << 20 {
                return Err(Error::protocol(format!(
                    "the server sent a {len}-byte message while the connection was being set up"
                )));
            }
            if let Some(body) = buf.body(len) {
                buf.consume(len);
                return Ok((tag, body));
            }
        }
        let n = stream
            .read_into(buf)
            .await
            .map_err(|e| Error::io(ErrorKind::Connect, "reading from the server failed", e))?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::Connect,
                "the server closed the connection while it was being set up",
            ));
        }
    }
}

async fn send(stream: &Stream, out: Vec<u8>) -> Result<(), Error> {
    let (r, _) = stream.write_all(out).await;
    r.map_err(|e| Error::io(ErrorKind::Connect, "writing to the server failed", e))
}

/// A server error while connecting: refused credentials are
/// [`ErrorKind::Auth`], anything else (no such database, too many
/// connections) [`ErrorKind::Connect`].
fn startup_error(config: &Config, body: &[u8]) -> Error {
    match DbError::parse(body) {
        Ok(db) => {
            let kind = if db.code().starts_with("28") {
                ErrorKind::Auth
            } else {
                ErrorKind::Connect
            };
            // PgBouncer's answer to a startup parameter it does not track
            // (its messages are not translated).
            let pooler =
                db.code() == "08P01" && db.message().contains("unsupported startup parameter");
            let message = if pooler && db.message().contains("statement_timeout") {
                format!(
                    "the connection pooler at {} refused the statement_timeout that \
                     giong-postgres sends when connecting. PgBouncer cannot pass it on to the \
                     server (track_extra_parameters accepts it without applying it, and \
                     ignore_startup_parameters drops it): call Config::statement_timeout(None) \
                     and set the limit on the role instead, ALTER ROLE ... SET \
                     statement_timeout = '30s'",
                    config.target()
                )
            } else if pooler {
                format!(
                    "the connection pooler at {} refused a setting given with Config::param: \
                     set it on the role instead (ALTER ROLE ... SET ...)",
                    config.target()
                )
            } else {
                format!("the server at {} refused the connection", config.target())
            };
            Error::with_db(kind, message, db)
        }
        Err(e) => e,
    }
}

/// Refuses an authentication method that [`Config::require_auth`] does not
/// allow, before anything is sent for it.
fn check_auth(config: &Config, method: AuthMethod) -> Result<(), Error> {
    match &config.require_auth {
        Some(allowed) if !allowed.contains(&method) => {
            let allowed: Vec<&str> = allowed.iter().map(|m| m.name()).collect();
            let what = match method {
                AuthMethod::None => "let the client in without authentication".to_owned(),
                m => format!("asked for {} authentication", m.name()),
            };
            Err(Error::new(
                ErrorKind::Auth,
                format!(
                    "the server at {} {what}, but require_auth allows only {}; refusing to go \
                     on, since this is not the server's configured method or not the real \
                     server. Fix pg_hba.conf, or change require_auth",
                    config.target(),
                    allowed.join(", ")
                ),
            ))
        }
        _ => Ok(()),
    }
}

/// The `statement_timeout` to send when connecting, in milliseconds.
fn statement_timeout_ms(config: &Config) -> Result<Option<String>, Error> {
    let Some(timeout) = config.statement_timeout else {
        return Ok(None);
    };
    let ms = timeout.as_micros().div_ceil(1000);
    if ms == 0 {
        return Err(Error::config(
            "Config::statement_timeout(Duration::ZERO) would let no statement run (PostgreSQL \
             reads 0 as no limit, which is not what a zero duration says); give a limit, or \
             Config::statement_timeout(None) to leave it to the server",
        ));
    }
    if ms > i32::MAX as u128 {
        return Err(Error::config(format!(
            "Config::statement_timeout({timeout:?}) is longer than PostgreSQL allows ({} ms, \
             about 24.8 days); give a shorter limit, or Config::statement_timeout(None) to leave \
             it to the server",
            i32::MAX
        )));
    }
    Ok(Some(ms.to_string()))
}

/// Sends the startup message and authenticates.
pub(crate) async fn startup(stream: Stream, config: &Config) -> Result<Startup, Error> {
    let user = config.user.as_deref().ok_or_else(|| {
        Error::config(
            "no user to log in as; put it in the URL (postgres://USER@host/db) or call Config::user",
        )
    })?;
    let timeout_ms = statement_timeout_ms(config)?;
    let mut params: Vec<(&str, &str)> = vec![("user", user), ("client_encoding", "UTF8")];
    if let Some(ms) = &timeout_ms {
        params.push(("statement_timeout", ms));
    }
    if let Some(db) = &config.dbname {
        params.push(("database", db));
    }
    if let Some(app) = &config.application_name {
        params.push(("application_name", app));
    }
    for (name, value) in &config.params {
        params.push((name, value));
    }
    let mut out = Vec::new();
    protocol::startup(&mut out, &params)?;
    send(&stream, out).await?;

    let encrypted = matches!(stream, Stream::Tls(_));
    let mut buf = ReadBuf::new();
    let mut scram: Option<Scram> = None;
    let mut method = AuthMethod::None;
    let password = || {
        config.password.as_deref().ok_or_else(|| {
            Error::new(
                ErrorKind::Auth,
                format!(
                    "the server asks for a password for user {user:?}, but none was given; put it \
                     in the URL (postgres://{user}:PASSWORD@host/db) or call Config::password"
                ),
            )
        })
    };
    loop {
        let (tag, range) = next_message(&stream, &mut buf).await?;
        let mut out = Vec::new();
        match parse_backend(tag, buf.bytes(range))? {
            Backend::AuthenticationOk => {
                check_auth(config, method)?;
                break;
            }
            Backend::AuthenticationCleartextPassword => {
                method = AuthMethod::Password;
                check_auth(config, method)?;
                if !encrypted && !config.is_local() {
                    return Err(Error::new(
                        ErrorKind::Auth,
                        format!(
                            "the server at {} asks for the password in clear text over a \
                             connection without TLS; refusing, since anyone on the network could \
                             read it. Turn on TLS (sslmode=verify-full), or use scram-sha-256 in \
                             the server's pg_hba.conf",
                            config.target()
                        ),
                    ));
                }
                let mut data = password()?.as_bytes().to_vec();
                data.push(0);
                protocol::password(&mut out, &data)?;
            }
            Backend::AuthenticationMd5Password(salt) => {
                method = AuthMethod::Md5;
                check_auth(config, method)?;
                let mut data = md5_password(user, password()?, salt).into_bytes();
                data.push(0);
                protocol::password(&mut out, &data)?;
            }
            Backend::AuthenticationSasl(mechanisms) => {
                if !mechanisms.contains(&"SCRAM-SHA-256") {
                    return Err(Error::new(
                        ErrorKind::Auth,
                        format!(
                            "the server offers only the SASL mechanisms {mechanisms:?}; \
                             giong-postgres supports SCRAM-SHA-256"
                        ),
                    ));
                }
                method = AuthMethod::ScramSha256;
                check_auth(config, method)?;
                let s = Scram::new(password()?)?;
                protocol::sasl_initial_response(&mut out, "SCRAM-SHA-256", &s.client_first())?;
                scram = Some(s);
            }
            Backend::AuthenticationSaslContinue(data) => {
                let s = scram.as_mut().ok_or_else(|| {
                    Error::protocol("the server continued a SASL exchange that was never started")
                })?;
                let (salt, iterations) = s.server_first(data)?;
                let salted = config
                    .scram_keys
                    .salted_password(s.password(), &salt, iterations)
                    .await;
                protocol::password(&mut out, &s.client_final_salted(&salted)?)?;
            }
            Backend::AuthenticationSaslFinal(data) => {
                scram
                    .as_mut()
                    .ok_or_else(|| {
                        Error::protocol(
                            "the server finished a SASL exchange that was never started",
                        )
                    })?
                    .finish(data)?;
            }
            Backend::AuthenticationOther(code) => {
                let method = match code {
                    2 => "Kerberos V5",
                    6 => "SCM credentials",
                    7 | 8 => "GSSAPI",
                    9 => "SSPI",
                    _ => "an unknown method",
                };
                return Err(Error::new(
                    ErrorKind::Auth,
                    format!(
                        "the server asks for {method} authentication (code {code}), which \
                         giong-postgres does not support; use scram-sha-256 in pg_hba.conf"
                    ),
                ));
            }
            Backend::ErrorResponse(body) => return Err(startup_error(config, body)),
            Backend::NoticeResponse | Backend::NegotiateProtocolVersion => {}
            _ => {
                return Err(Error::protocol(format!(
                    "the server sent an unexpected {:?} message during authentication",
                    tag as char
                )))
            }
        }
        if !out.is_empty() {
            send(&stream, out).await?;
        }
    }
    if scram.as_ref().is_some_and(|s| !s.is_done()) {
        return Err(Error::new(
            ErrorKind::Auth,
            "the server ended SCRAM authentication without proving that it knows the password; \
             refusing to continue",
        ));
    }

    let mut params = Vec::new();
    let mut key = None;
    loop {
        let (tag, range) = next_message(&stream, &mut buf).await?;
        match parse_backend(tag, buf.bytes(range))? {
            Backend::KeyData {
                process_id,
                secret_key,
            } => key = Some((process_id, secret_key)),
            Backend::ParameterStatus { name, value } => {
                params.push((name.to_owned(), value.to_owned()));
            }
            Backend::ReadyForQuery(status) => {
                return Ok(Startup {
                    stream,
                    buf,
                    params,
                    key,
                    status,
                })
            }
            Backend::ErrorResponse(body) => return Err(startup_error(config, body)),
            Backend::NoticeResponse | Backend::NegotiateProtocolVersion => {}
            _ => {
                return Err(Error::protocol(format!(
                    "the server sent an unexpected {:?} message after authentication",
                    tag as char
                )))
            }
        }
    }
}

/// Opens a connection and logs in, within the connect timeout.
pub(crate) async fn connect(config: &Config) -> Result<Startup, Error> {
    let limit = config.connect_timeout;
    let attempt = async { startup(open(config).await?, config).await };
    match giong::time::timeout(limit, attempt).await {
        Ok(result) => result,
        Err(_) => Err(Error::new(
            ErrorKind::Connect,
            format!(
                "connecting to {} took longer than the connect timeout ({limit:?}); check that \
                 the server is up and reachable, or raise Config::connect_timeout",
                config.target()
            ),
        )),
    }
}

/// Asks the server to cancel what the connection with this key is running.
pub(crate) async fn cancel(config: &Config, key: (i32, i32)) -> Result<(), Error> {
    let attempt = async {
        let stream = open(config).await?;
        let mut out = Vec::with_capacity(16);
        protocol::cancel_request(&mut out, key.0, key.1);
        send(&stream, out).await?;
        // The server closes the connection once it has read the request.
        loop {
            match stream.recv().await {
                Ok(chunk) if !chunk.is_empty() => continue,
                _ => return Ok(()),
            }
        }
    };
    let limit = config.connect_timeout;
    giong::time::timeout(limit, attempt)
        .await
        .unwrap_or_else(|_| {
            Err(Error::new(
                ErrorKind::Connect,
                format!(
                "sending the cancel request to {} took longer than the connect timeout ({limit:?})",
                config.target()
            ),
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `chunk` the way a receive into lent storage does: the kernel
    /// writes up to the room it is given, so a chunk may take several.
    fn feed_lent(buf: &mut ReadBuf, mut chunk: &[u8], room: usize) {
        use giong::IoBufMut;
        while !chunk.is_empty() {
            let mut tail = buf.lend(room);
            assert!(tail.capacity() >= room);
            let n = chunk.len().min(tail.capacity());
            // SAFETY: `n` fits in the room lent, and the bytes are written
            // before they are marked.
            unsafe {
                std::ptr::copy_nonoverlapping(chunk.as_ptr(), tail.as_mut_ptr(), n);
                tail.set_init(n);
            }
            assert_eq!(giong::IoBuf::len(&tail), n);
            buf.filled(tail);
            chunk = &chunk[n..];
        }
    }

    /// A message of `len` bytes in all, tagged `tag`, filled with `fill`.
    fn message(tag: u8, len: usize, fill: u8) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend_from_slice(&(len as u32 - 1).to_be_bytes());
        out.resize(len, fill);
        out
    }

    #[test]
    fn a_receive_into_lent_storage_reads_what_copying_reads() {
        let mut input = message(b'D', 20, 1);
        input.extend(message(b'D', 3 << 20, 2));
        input.extend(message(b'C', 13, 3));
        for room in [1, 5, 4096, 64 * 1024] {
            for lent in [false, true] {
                let mut buf = ReadBuf::new();
                let mut seen = Vec::new();
                for chunk in input.chunks(room.max(1000)) {
                    if lent {
                        feed_lent(&mut buf, chunk, room);
                    } else {
                        buf.extend(chunk);
                    }
                    while let Some((tag, len)) = buf.peek().unwrap() {
                        if len > 1 << 20 {
                            // Thrown away as it lands, like an oversized row.
                            buf.skip(len);
                            seen.push((tag, len, None));
                            continue;
                        }
                        let Some(body) = buf.body(len) else { break };
                        seen.push((tag, len, Some(buf.bytes(body).to_vec())));
                        buf.consume(len);
                    }
                    assert!(
                        buf.data.len() < 1 << 20,
                        "room {room}: the big message was kept"
                    );
                }
                assert_eq!(
                    seen,
                    [
                        (b'D', 20, Some(vec![1; 15])),
                        (b'D', 3 << 20, None),
                        (b'C', 13, Some(vec![3; 8])),
                    ],
                    "room {room}, lent {lent}"
                );
            }
        }
    }
}
