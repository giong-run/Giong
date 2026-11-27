//! Opening a connection: the socket, the server's greeting, TLS, and
//! authentication.

use std::io;
use std::net::{Shutdown, SocketAddr};
use std::ops::{Deref, Range};
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, BorrowedSocket};
use std::time::Duration;

use giong::net::{TcpStream, UnixStream};
use giong::RecvBuf;
use giong_tls::TlsStream;
use socket2::{SockRef, TcpKeepalive};
use zeroize::{Zeroize, Zeroizing};

use std::rc::Rc;

use crate::auth;
use crate::config::{Config, Host};
use crate::error::{DbError, Error, ErrorKind};
use crate::protocol::{self, cap, header, HandshakeResponse, MAX_PAYLOAD};

/// Which server it is: MySQL (8.0 and later, and its forks), or MariaDB
/// (10.6 and later), whose session variables and collations differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Server {
    MySql,
    MariaDb,
}

impl Server {
    /// Reads the version from the greeting: the kind of server, and the
    /// version as it should be shown (MariaDB before 11 puts `5.5.5-` in
    /// front, for old clients). Servers too old for giong-mysql are refused.
    fn from_version(config: &Config, version: &str) -> Result<(Server, String), Error> {
        let (server, shown) = if version.contains("MariaDB") {
            let shown = version.strip_prefix("5.5.5-").unwrap_or(version);
            (Server::MariaDb, shown.to_owned())
        } else {
            (Server::MySql, version.to_owned())
        };
        let mut numbers = shown
            .split(|c: char| !c.is_ascii_digit())
            .map(|n| n.parse::<u32>().unwrap_or(0));
        let major = numbers.next().unwrap_or(0);
        let minor = numbers.next().unwrap_or(0);
        let too_old = match server {
            Server::MySql => major < 8,
            Server::MariaDb => (major, minor) < (10, 6),
        };
        if too_old {
            return Err(Error::new(
                ErrorKind::Connect,
                format!(
                    "the server at {} is {shown}; giong-mysql works with MySQL 8.0 and later and \
                     MariaDB 10.6 and later",
                    config.target()
                ),
            ));
        }
        Ok((server, shown))
    }

    /// The collation the handshake asks for: one the server knows.
    fn handshake_collation(self) -> u8 {
        match self {
            Server::MySql => protocol::UTF8MB4_0900_AI_CI,
            Server::MariaDb => protocol::UTF8MB4_GENERAL_CI,
        }
    }

    /// What is read from the server after the session is set up: the
    /// character set it really uses, whether a lock wait timeout rolls back
    /// the whole transaction, and which server it is (for
    /// [`kill_query`]).
    fn facts_sql(self) -> &'static str {
        match self {
            Server::MySql => {
                "SELECT @@character_set_client, @@character_set_connection, \
                 @@character_set_results, @@innodb_rollback_on_timeout, @@server_uuid"
            }
            // MariaDB has no server_uuid (measured on 10.11); host name,
            // port and server id tell its servers apart.
            Server::MariaDb => {
                "SELECT @@character_set_client, @@character_set_connection, \
                 @@character_set_results, @@innodb_rollback_on_timeout, \
                 CONCAT(@@hostname, ':', @@port, ':', @@server_id)"
            }
        }
    }
}

/// A dead peer is noticed after about a minute: keepalive probes start after
/// 30 s of silence and give up after three more 10 s apart, and data the
/// server has not acknowledged for 60 s fails the connection.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_RETRIES: u32 = 3;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const USER_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest packet accepted while the connection is being set up.
const SETUP_LIMIT: usize = 1 << 20;

/// Free room asked of the read buffer before each receive: replies that
/// arrive together are taken in one receive up to this much.
const READ_ROOM: usize = 64 * 1024;

/// The socket under a connection.
pub(crate) enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
    Tls(Box<TlsStream>),
    /// A server played from bytes, for the fuzz targets and the tests.
    #[cfg(any(test, fuzzing))]
    Memory(crate::fuzzing::Memory),
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
            #[cfg(any(test, fuzzing))]
            Stream::Memory(s) => s.recv().await.map(Chunk::Owned),
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
            _ => {
                let chunk = self.recv().await?;
                buf.extend(&chunk);
                return Ok(chunk.len());
            }
        };
        let (result, tail) = match self {
            Stream::Tcp(s) => s.read(tail).await,
            Stream::Unix(s) => s.read(tail).await,
            _ => unreachable!("only sockets read in place"),
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
            #[cfg(any(test, fuzzing))]
            Stream::Memory(s) => s.write_all(buf).await,
        }
    }

    /// Shuts the socket down in `how`, which also wakes a task waiting in
    /// `recv`.
    pub(crate) fn shutdown(&self, how: Shutdown) {
        let _ = match self {
            Stream::Tcp(s) => s.shutdown(how),
            Stream::Unix(s) => s.shutdown(how),
            Stream::Tls(s) => s.tcp().shutdown(how),
            #[cfg(any(test, fuzzing))]
            Stream::Memory(s) => {
                s.shutdown(how);
                Ok(())
            }
        };
    }

    /// Whether a password may cross this connection in clear text: it is
    /// encrypted, or it never leaves the machine.
    fn is_secure(&self) -> bool {
        matches!(self, Stream::Tls(_) | Stream::Unix(_))
    }
}

/// One message: a packet, or packets of 16 MiB - 1 bytes joined with the
/// shorter one that ends them.
pub(crate) enum Message {
    /// A packet whole in the buffer: its payload is at this range, and
    /// [`ReadBuf::consume`] moves past it.
    Here { seq: u8, payload: Range<usize> },
    /// A message that was joined from several packets (already consumed):
    /// `seq` is the number of its first packet, `next` the number the packet
    /// after its last must have.
    Joined { seq: u8, next: u8, payload: Vec<u8> },
    /// A message longer than the limit, thrown away as it arrived (already
    /// consumed), numbered as [`Joined`](Self::Joined) is.
    TooLarge { seq: u8, next: u8 },
    /// A packet in the middle of a long message had the wrong number.
    OutOfOrder { expected: u8, got: u8 },
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
    /// Bytes of a packet being thrown away that have not arrived yet.
    skip: usize,
    /// The long message in progress is being thrown away.
    skipping: bool,
    /// The packets of a long message joined so far.
    joined: Vec<u8>,
    /// The number of the first packet of the long message in progress.
    long: Option<u8>,
    /// The number its next packet must have.
    long_next: u8,
}

impl ReadBuf {
    pub(crate) fn new() -> Self {
        Self {
            data: Vec::with_capacity(8192),
            start: 0,
            skip: 0,
            skipping: false,
            joined: Vec::new(),
            long: None,
            long_next: 0,
        }
    }

    pub(crate) fn extend(&mut self, mut chunk: &[u8]) {
        if self.skip > 0 {
            let n = self.skip.min(chunk.len());
            self.skip -= n;
            chunk = &chunk[n..];
        }
        self.make_room(chunk.len() < 8192);
        self.data.extend_from_slice(chunk);
    }

    /// Lends the free end of the storage, with room for at least `want`
    /// bytes, to a receive that writes straight into it (no copy through a
    /// pooled buffer). Give it back with [`filled`](Self::filled).
    pub(crate) fn lend(&mut self, want: usize) -> Tail {
        self.make_room(true);
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

    /// Drops the bytes already taken apart, when that is cheap; `shrink`
    /// gives back a buffer one large message grew, once it is empty.
    fn make_room(&mut self, shrink: bool) {
        if self.start == self.data.len() {
            self.data.clear();
            self.start = 0;
            // A large row once must not keep every pooled connection holding
            // a buffer its size.
            if self.data.capacity() > crate::client::KEEP_CAPACITY && shrink {
                self.data = Vec::with_capacity(8192);
            }
        } else if self.start > 0 && self.start >= self.data.len() / 2 {
            self.data.drain(..self.start);
            self.start = 0;
        }
    }

    /// Nothing is held: no bytes, not even part of a packet.
    fn is_empty(&self) -> bool {
        self.start == self.data.len() && self.skip == 0 && self.long.is_none()
    }

    /// Throws away a packet of `len` payload bytes at the front, including
    /// the part that has not arrived yet.
    fn drop_packet(&mut self, len: usize) {
        let total = len + 4;
        let here = (self.data.len() - self.start).min(total);
        self.start += here;
        self.skip = total - here;
    }

    /// The next whole message, if it is here. Messages longer than `limit`
    /// are thrown away and reported as [`Message::TooLarge`].
    pub(crate) fn next(&mut self, limit: usize) -> Option<Message> {
        loop {
            if self.skip > 0 {
                return None;
            }
            let (len, seq) = header(&self.data[self.start..])?;
            let first = match self.long {
                None if len < MAX_PAYLOAD => {
                    if len > limit {
                        self.drop_packet(len);
                        let next = seq.wrapping_add(1);
                        return Some(Message::TooLarge { seq, next });
                    }
                    if self.data.len() - self.start < len + 4 {
                        return None;
                    }
                    let payload = self.start + 4..self.start + 4 + len;
                    return Some(Message::Here { seq, payload });
                }
                // The first packet of a long message.
                None => {
                    self.long = Some(seq);
                    self.long_next = seq;
                    seq
                }
                Some(first) => first,
            };
            if seq != self.long_next {
                let expected = self.long_next;
                self.long = None;
                self.joined = Vec::new();
                self.skipping = false;
                return Some(Message::OutOfOrder { expected, got: seq });
            }
            if !self.skipping && self.joined.len() + len > limit {
                self.joined = Vec::new();
                self.skipping = true;
            }
            if self.skipping {
                self.drop_packet(len);
            } else {
                if self.data.len() - self.start < len + 4 {
                    return None;
                }
                self.joined
                    .extend_from_slice(&self.data[self.start + 4..self.start + 4 + len]);
                self.start += len + 4;
            }
            self.long_next = seq.wrapping_add(1);
            // A packet shorter than the maximum ends the message.
            if len < MAX_PAYLOAD {
                self.long = None;
                let next = self.long_next;
                return Some(if std::mem::take(&mut self.skipping) {
                    Message::TooLarge { seq: first, next }
                } else {
                    Message::Joined {
                        seq: first,
                        next,
                        payload: std::mem::take(&mut self.joined),
                    }
                });
            }
        }
    }

    pub(crate) fn bytes(&self, range: Range<usize>) -> &[u8] {
        &self.data[range]
    }

    /// Moves past a [`Message::Here`] whose payload is at `range`.
    pub(crate) fn consume(&mut self, range: &Range<usize>) {
        self.start = range.end;
    }
}

/// What the server said while the connection was being set up.
pub(crate) struct Startup {
    pub stream: Stream,
    pub buf: ReadBuf,
    pub connection_id: u32,
    pub server_version: String,
    pub status: u16,
    /// The `SET` that set the session up, run again after a reset.
    pub session_sql: Rc<str>,
    /// Who to ask to cancel this connection's statements.
    pub identity: Identity,
    /// `innodb_rollback_on_timeout`: a lock wait timeout (1205) rolls back
    /// the whole transaction, not only the statement.
    pub rollback_on_timeout: bool,
}

/// Where a connection went, so that [`kill_query`] reaches the same server
/// and makes sure of it before killing anything.
#[derive(Clone, Debug)]
pub(crate) struct Identity {
    /// The address the connection reached, for TCP; a name the
    /// configuration gives can resolve to another server next time.
    pub peer: Option<SocketAddr>,
    /// `@@server_uuid` on MySQL; host name, port and server id on MariaDB.
    pub server_id: String,
}

fn connect_error(config: &Config, message: impl std::fmt::Display, e: io::Error) -> Error {
    Error::io(
        ErrorKind::Connect,
        format!("cannot connect to {}: {message}", config.target()),
        e,
    )
}

/// Turns off Nagle's algorithm (requests are written whole) and sets up
/// keepalive, so that a server that vanished is noticed.
fn tune(tcp: &TcpStream) -> io::Result<()> {
    tcp.set_nodelay(true)?;
    // SAFETY: the socket belongs to `tcp`, which outlives this borrow.
    #[cfg(unix)]
    let fd = unsafe { BorrowedFd::borrow_raw(tcp.as_raw_fd()) };
    // SAFETY: as above.
    #[cfg(windows)]
    let fd = unsafe { BorrowedSocket::borrow_raw(tcp.as_raw_socket()) };
    let sock = SockRef::from(&fd);
    sock.set_tcp_keepalive(
        &TcpKeepalive::new()
            .with_time(KEEPALIVE_IDLE)
            .with_interval(KEEPALIVE_INTERVAL)
            .with_retries(KEEPALIVE_RETRIES),
    )?;
    // Only Linux and its relatives have TCP_USER_TIMEOUT; elsewhere the
    // keepalive probes alone notice a vanished server.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    sock.set_tcp_user_timeout(Some(USER_TIMEOUT))?;
    Ok(())
}

/// A connection being set up: the stream, what was read, and the sequence
/// number of the next packet.
struct Setup {
    stream: Stream,
    buf: ReadBuf,
    seq: u8,
}

impl Setup {
    /// The next packet's payload, which must carry the expected sequence
    /// number.
    async fn read(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            match self.buf.next(SETUP_LIMIT) {
                Some(Message::Here { seq, payload }) => {
                    let bytes = self.buf.bytes(payload.clone()).to_vec();
                    self.buf.consume(&payload);
                    self.check_seq(seq)?;
                    return Ok(bytes);
                }
                Some(
                    Message::Joined { .. } | Message::TooLarge { .. } | Message::OutOfOrder { .. },
                ) => {
                    return Err(Error::protocol(
                        "the server sent a packet of more than 1 MiB while the connection was \
                         being set up",
                    ))
                }
                None => {}
            }
            let n =
                self.stream.read_into(&mut self.buf).await.map_err(|e| {
                    Error::io(ErrorKind::Connect, "reading from the server failed", e)
                })?;
            if n == 0 {
                return Err(Error::new(
                    ErrorKind::Connect,
                    "the server closed the connection while it was being set up",
                ));
            }
        }
    }

    fn check_seq(&mut self, seq: u8) -> Result<(), Error> {
        if seq != self.seq {
            return Err(Error::protocol(format!(
                "the server sent packet number {seq} where {} was due",
                self.seq
            )));
        }
        self.seq = seq.wrapping_add(1);
        Ok(())
    }

    /// Reads the answer to a text query (`COM_QUERY`) that returns one row,
    /// as text; `None` for each `NULL`.
    async fn text_row(&mut self, what: &str) -> Result<Vec<Option<String>>, Error> {
        let first = self.read().await?;
        if first.first() == Some(&0xFF) {
            return Err(Error::from_db(
                DbError::parse(&first)?,
                Some(what.to_owned()),
            ));
        }
        let columns = protocol::Reader::new(&first).lenenc("a column count")?;
        if columns == 0 || columns > 64 {
            return Err(Error::protocol(format!(
                "the server answered {what} with no row"
            )));
        }
        for _ in 0..columns {
            self.read().await?;
        }
        let mut row = None;
        loop {
            let packet = self.read().await?;
            match packet.first() {
                Some(0xFF) => {
                    return Err(Error::from_db(
                        DbError::parse(&packet)?,
                        Some(what.to_owned()),
                    ))
                }
                _ if protocol::is_end_of_rows(&packet) => break,
                _ if row.is_some() => {
                    return Err(Error::protocol(format!(
                        "the server answered {what} with more than one row"
                    )))
                }
                _ => {
                    let mut r = protocol::Reader::new(&packet);
                    let mut values = Vec::new();
                    for _ in 0..columns {
                        if r.rest().first() == Some(&0xFB) {
                            r.u8(what)?;
                            values.push(None);
                        } else {
                            let v = r.lenenc_bytes(what)?;
                            values.push(Some(String::from_utf8_lossy(v).into_owned()));
                        }
                    }
                    row = Some(values);
                }
            }
        }
        row.ok_or_else(|| Error::protocol(format!("the server answered {what} with no row")))
    }

    /// Sends one packet with the next sequence number.
    async fn send(&mut self, body: impl FnOnce(&mut Vec<u8>)) -> Result<(), Error> {
        let mut out = Vec::new();
        self.seq = protocol::packet(&mut out, self.seq, body);
        self.write(out).await
    }

    async fn write(&self, out: Vec<u8>) -> Result<(), Error> {
        let (r, _) = self.stream.write_all(out).await;
        r.map_err(|e| Error::io(ErrorKind::Connect, "writing to the server failed", e))
    }

    /// [`send`](Self::send) for a packet that holds the password: the
    /// buffer, sized so that it never moves, is wiped once written.
    async fn send_secret(&mut self, secret: &[u8]) -> Result<(), Error> {
        let mut out = Vec::with_capacity(4 + secret.len());
        self.seq = protocol::packet(&mut out, self.seq, |out| out.extend_from_slice(secret));
        let (r, mut out) = self.stream.write_all(out).await;
        out.zeroize();
        r.map_err(|e| Error::io(ErrorKind::Connect, "writing to the server failed", e))
    }
}

/// A server error while connecting: refused credentials are
/// [`ErrorKind::Auth`], anything else (no such database, too many
/// connections) [`ErrorKind::Connect`].
fn startup_error(config: &Config, payload: &[u8]) -> Error {
    match DbError::parse(payload) {
        Ok(db) => {
            // 1045 access denied, 1044 no access to the database, 1698
            // access denied without a password, 1251 authentication method,
            // 1130 host not allowed to connect, 1862 password expired.
            let auth = db.sqlstate() == "28000"
                || matches!(db.code(), 1044 | 1045 | 1251 | 1130 | 1698 | 1862);
            let kind = if auth {
                ErrorKind::Auth
            } else {
                ErrorKind::Connect
            };
            Error::with_db(
                kind,
                format!("the server at {} refused the connection", config.target()),
                db,
            )
        }
        Err(e) => e,
    }
}

/// Opens the socket, reads the server's greeting, and starts TLS when the
/// configuration asks for it.
async fn open(
    config: &Config,
    capabilities: u32,
) -> Result<(Setup, protocol::Handshake, u32, Server, Option<SocketAddr>), Error> {
    let (stream, tls, peer) = match &config.host {
        Host::Unix(path) => {
            let stream = UnixStream::connect(path).await.map_err(|e| {
                connect_error(
                    config,
                    "is the server running, and is this its socket (see its `socket` setting)?",
                    e,
                )
            })?;
            (Stream::Unix(stream), None, None)
        }
        Host::Tcp(host) => {
            let tls = config.tls_connector()?;
            let tcp = TcpStream::connect((host.as_str(), config.port))
                .await
                .map_err(|e| connect_error(config, "is the server running and reachable?", e))?;
            tune(&tcp).map_err(|e| connect_error(config, "cannot set socket options", e))?;
            let peer = tcp.peer_addr().ok();
            let name = config.tls_name.clone().unwrap_or_else(|| host.clone());
            (Stream::Tcp(tcp), tls.map(|c| (c, name)), peer)
        }
    };
    let mut setup = Setup {
        stream,
        buf: ReadBuf::new(),
        seq: 0,
    };
    let greeting = setup.read().await?;
    if greeting.first() == Some(&0xFF) {
        return Err(startup_error(config, &greeting));
    }
    let mut hello = protocol::handshake(&greeting)?;
    let (server, version) = Server::from_version(config, &hello.server_version)?;
    hello.server_version = version;
    let missing = protocol::REQUIRED & !hello.capabilities;
    if missing != 0 {
        return Err(Error::new(
            ErrorKind::Connect,
            format!(
                "the server at {} ({}) lacks protocol features giong-mysql needs (capability \
                 bits 0x{missing:08X}); it works with MySQL 8.0 and later and MariaDB 10.6 and \
                 later",
                config.target(),
                hello.server_version
            ),
        ));
    }
    let mut caps = capabilities & hello.capabilities | protocol::REQUIRED;
    // Decided before the TLS request: the login after it must carry the same
    // flags, or the server refuses it ("Bad handshake").
    if config.database.is_some() {
        caps |= cap::CONNECT_WITH_DB;
    }
    let Some((connector, host)) = tls else {
        return Ok((setup, hello, caps, server, peer));
    };
    if hello.capabilities & cap::SSL == 0 {
        return Err(Error::new(
            ErrorKind::Connect,
            format!(
                "the server at {} does not accept TLS; turn it on there (MySQL 8 does by \
                 default), or add ssl-mode=DISABLED to the URL if the network is trusted",
                config.target()
            ),
        ));
    }
    caps |= cap::SSL;
    // Nothing else can be buffered: the server waits for this request.
    // Bytes held now, even part of a packet, would be read after the
    // handshake as if they had come through TLS, so someone on the path
    // could slip them in (the flaw of CVE-2021-23222 in PostgreSQL clients).
    if !setup.buf.is_empty() {
        return Err(Error::protocol(
            "the server sent more than its greeting before the TLS request",
        ));
    }
    let mut out = Vec::new();
    protocol::ssl_request(&mut out, caps, server.handshake_collation());
    setup.seq = 2;
    setup.write(out).await?;
    let Stream::Tcp(tcp) = setup.stream else {
        unreachable!("TLS is only used over TCP")
    };
    let tls = connector
        .connect(&host, tcp)
        .await
        .map_err(|e| connect_error(config, format!("the TLS handshake with {host} failed"), e))?;
    setup.stream = Stream::Tls(Box::new(tls));
    Ok((setup, hello, caps, server, peer))
}

/// What to answer for an authentication plugin's first request.
/// The password with a NUL after it, as the clear-text exchanges send it:
/// sized so that nothing moves, and wiped when dropped.
fn nul_terminated(password: &str) -> Zeroizing<Vec<u8>> {
    let mut p = Zeroizing::new(Vec::with_capacity(password.len() + 1));
    p.extend_from_slice(password.as_bytes());
    p.push(0);
    p
}

fn first_response(plugin: &str, password: &str, nonce: &[u8]) -> Option<Vec<u8>> {
    match plugin {
        "caching_sha2_password" => Some(auth::caching_sha2(password, nonce)),
        "mysql_native_password" => Some(auth::native(password, nonce)),
        _ => None,
    }
}

/// Logs in on a connection whose greeting has been read.
async fn authenticate(
    config: &Config,
    setup: &mut Setup,
    hello: &protocol::Handshake,
    caps: u32,
    server: Server,
) -> Result<(), Error> {
    let user = config.user.as_deref().ok_or_else(|| {
        Error::config(
            "no user to log in as; put it in the URL (mysql://USER@host/db) or call Config::user",
        )
    })?;
    let password = config.password.as_deref().unwrap_or("");
    // Start with the server's default plugin when this crate knows it, so
    // that a login usually takes one round trip.
    let mut plugin = hello.plugin.clone();
    let mut nonce = hello.nonce.clone();
    let response = match first_response(&plugin, password, &nonce) {
        Some(r) => r,
        None => {
            plugin = "caching_sha2_password".to_owned();
            auth::caching_sha2(password, &nonce)
        }
    };
    let version = env!("CARGO_PKG_VERSION");
    let mut attrs = vec![
        ("_client_name", "giong-mysql"),
        ("_client_version", version),
        ("_os", std::env::consts::OS),
    ];
    if let Some(app) = &config.application_name {
        attrs.push(("program_name", app));
    }
    let request = HandshakeResponse {
        capabilities: caps,
        collation: server.handshake_collation(),
        user,
        auth: &response,
        database: config.database.as_deref(),
        plugin: &plugin,
        attrs: &attrs,
    };
    let mut out = Vec::new();
    setup.seq = protocol::handshake_response(&mut out, setup.seq, &request);
    setup.write(out).await?;

    let secure = setup.stream.is_secure();
    let mut asked_for_key = false;
    loop {
        let packet = setup.read().await?;
        match packet.first() {
            Some(0x00) => return Ok(()),
            Some(0xFF) => return Err(startup_error(config, &packet)),
            Some(0xFE) => {
                // AuthSwitchRequest: another plugin, with a fresh nonce.
                let mut r = protocol::Reader::new(&packet[1..]);
                let name = r.nul_str("an authentication switch")?;
                plugin = String::from_utf8_lossy(name).into_owned();
                let data = r.rest();
                nonce = data.strip_suffix(&[0]).unwrap_or(data).to_vec();
                let response = match plugin.as_str() {
                    "mysql_clear_password" | "sha256_password" if secure => {
                        nul_terminated(password)
                    }
                    "mysql_clear_password" | "sha256_password" => {
                        return Err(Error::new(
                            ErrorKind::Auth,
                            format!(
                                "the server asks for the password of {user:?} with {plugin} over \
                                 a connection without TLS; refusing to send it. Turn on TLS, or \
                                 use caching_sha2_password for this user"
                            ),
                        ))
                    }
                    other => first_response(other, password, &nonce)
                        .map(Zeroizing::new)
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::Auth,
                                format!(
                                    "the server asks for the {other} authentication plugin for \
                                 {user:?}, which giong-mysql does not support; use \
                                 caching_sha2_password (MySQL 8's default)"
                                ),
                            )
                        })?,
                };
                setup.send_secret(&response).await?;
            }
            Some(0x01) if plugin == "caching_sha2_password" => {
                match packet.get(1..) {
                    // Fast authentication worked; an OK packet follows.
                    Some([0x03]) => {}
                    // The server has no cached hash: it needs the password.
                    Some([0x04]) => {
                        if secure {
                            setup.send_secret(&nul_terminated(password)).await?;
                        } else if let Some(pem) = &config.server_public_key {
                            let sealed = auth::rsa_encrypt_password(password, &nonce, pem)?;
                            setup.send(|out| out.extend_from_slice(&sealed)).await?;
                        } else if config.is_local() {
                            asked_for_key = true;
                            setup.send(|out| out.push(0x02)).await?;
                        } else {
                            return Err(Error::new(
                                ErrorKind::Auth,
                                format!(
                                    "the server at {} needs the password of {user:?} itself (it \
                                     has not seen the user since it started), and the connection \
                                     has no TLS. Turn on TLS (ssl-mode=VERIFY_IDENTITY), or give \
                                     the server's RSA key with Config::server_public_key; asking \
                                     the server for its key over the network would let anyone in \
                                     between read the password",
                                    config.target()
                                ),
                            ));
                        }
                    }
                    Some(pem) if asked_for_key => {
                        asked_for_key = false;
                        let sealed = auth::rsa_encrypt_password(password, &nonce, pem)?;
                        setup.send(|out| out.extend_from_slice(&sealed)).await?;
                    }
                    _ => {
                        return Err(Error::protocol(
                            "the server sent an unexpected caching_sha2_password message",
                        ))
                    }
                }
            }
            _ => {
                return Err(Error::protocol(format!(
                    "the server sent an unexpected packet during authentication (it starts with \
                     {:?})",
                    packet.first()
                )))
            }
        }
    }
}

/// Opens a connection, logs in and sets up the session.
async fn startup(config: &Config) -> Result<Startup, Error> {
    let (mut setup, hello, caps, server, peer) =
        open(config, protocol::REQUIRED | protocol::WANTED).await?;
    authenticate(config, &mut setup, &hello, caps, server).await?;

    // The session (the character set, UTC, the time limits and the
    // caller's variables), then what the server makes of it, in one round
    // trip.
    let sql = config.session_sql(server)?;
    let mut out = Vec::new();
    protocol::command_text(&mut out, protocol::com::QUERY, &sql);
    protocol::command_text(&mut out, protocol::com::QUERY, server.facts_sql());
    setup.write(out).await?;
    setup.seq = 1;
    let answer = setup.read().await?;
    let status = match answer.first() {
        Some(0x00) => protocol::ok(&answer)?.status,
        Some(0xFF) => {
            let db = DbError::parse(&answer)?;
            return Err(Error::with_db(
                ErrorKind::Config,
                format!("setting up the session failed ({sql})"),
                db,
            ));
        }
        _ => {
            return Err(Error::protocol(
                "the server answered SET with something else",
            ))
        }
    };
    setup.seq = 1;
    let facts = setup.text_row("the session check").await?;
    let fact = |i: usize| facts.get(i).cloned().flatten().unwrap_or_default();
    // A proxy between the two can still undo SET NAMES; text would then be
    // stored in another character set without any error.
    let charsets = [fact(0), fact(1), fact(2)];
    if charsets.iter().any(|c| c != "utf8mb4") {
        return Err(Error::new(
            ErrorKind::Connect,
            format!(
                "the server at {} uses the character sets {charsets:?} for this connection (client, \
                 connection, results) after SET NAMES utf8mb4; giong-mysql sends and reads \
                 UTF-8, which would be stored converted. Look for a proxy that changes the \
                 character set",
                config.target()
            ),
        ));
    }
    Ok(Startup {
        stream: setup.stream,
        buf: setup.buf,
        connection_id: hello.connection_id,
        server_version: hello.server_version,
        status,
        session_sql: sql.into(),
        identity: Identity {
            peer,
            server_id: fact(4),
        },
        rollback_on_timeout: fact(3) == "1",
    })
}

/// Opens a connection and logs in, within the connect timeout.
pub(crate) async fn connect(config: &Config) -> Result<Startup, Error> {
    let limit = config.connect_timeout;
    match giong::time::timeout(limit, startup(config)).await {
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

/// Asks the server, over a connection of its own, to stop the statement
/// that connection `id` is running.
///
/// Connection ids are unique only within one server. Behind DNS round
/// robin or a load balancer, a new connection to the same name can reach
/// another server, where `id` belongs to some other session: the new
/// connection goes to the address the first one reached, and `KILL` is sent
/// only if the server there is the same one.
pub(crate) async fn kill_query(config: &Config, identity: &Identity, id: u32) -> Result<(), Error> {
    let attempt = async {
        let mut config = config.clone();
        if let (Some(peer), Host::Tcp(name)) = (identity.peer, &config.host) {
            config.tls_name = Some(name.clone());
            config.host = Host::Tcp(peer.ip().to_string());
            config.port = peer.port();
        }
        let config = &config;
        let startup = startup(config).await?;
        if startup.identity.server_id != identity.server_id {
            return Err(Error::new(
                ErrorKind::Connect,
                format!(
                    "the cancel request reached a different server ({}) than the connection it is \
                     for ({}), as can happen behind a load balancer; KILL QUERY was not sent, since \
                     connection {id} there is someone else's. Cancel by dropping the statement's \
                     future and closing its connection instead",
                    startup.identity.server_id, identity.server_id
                ),
            ));
        }
        let mut setup = Setup {
            stream: startup.stream,
            buf: startup.buf,
            seq: 0,
        };
        let mut out = Vec::new();
        setup.seq =
            protocol::command_text(&mut out, protocol::com::QUERY, &format!("KILL QUERY {id}"));
        protocol::command(&mut out, protocol::com::QUIT);
        setup.write(out).await?;
        let answer = setup.read().await?;
        match answer.first() {
            Some(0x00) => Ok(()),
            // 1094: the connection is gone already; nothing to stop.
            Some(0xFF) => {
                let db = DbError::parse(&answer)?;
                if db.code() == 1094 {
                    Ok(())
                } else {
                    Err(Error::from_db(db, Some("KILL QUERY failed".to_owned())))
                }
            }
            _ => Err(Error::protocol(
                "the server answered KILL QUERY with something else",
            )),
        }
    };
    let limit = config.connect_timeout;
    giong::time::timeout(limit, attempt)
        .await
        .unwrap_or_else(|_| {
            Err(Error::new(
                ErrorKind::Connect,
                format!(
                    "asking {} to cancel took longer than the connect timeout ({limit:?})",
                    config.target()
                ),
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_told_apart_and_old_ones_refused_by_version() {
        let config = Config::new().host("db").user("u");
        let server = |v: &str| Server::from_version(&config, v);
        let (s, shown) = server("8.0.46-0ubuntu0.24.04.5").unwrap();
        assert_eq!(
            (s, shown.as_str()),
            (Server::MySql, "8.0.46-0ubuntu0.24.04.5")
        );
        assert_eq!(server("9.1.0").unwrap().0, Server::MySql);
        // MariaDB 10 puts 5.5.5- in front, for old clients; 11 does not.
        let (s, shown) = server("5.5.5-10.11.14-MariaDB-0ubuntu0.24.04.1").unwrap();
        assert_eq!(
            (s, shown.as_str()),
            (Server::MariaDb, "10.11.14-MariaDB-0ubuntu0.24.04.1")
        );
        assert_eq!(server("5.5.5-10.6.20-MariaDB").unwrap().0, Server::MariaDb);
        assert_eq!(server("11.4.4-MariaDB-ubu2404").unwrap().0, Server::MariaDb);
        for old in [
            "5.7.44-log",
            "5.6.51",
            "5.5.5-10.5.27-MariaDB",
            "10.4.34-MariaDB",
            "",
        ] {
            let e = server(old).unwrap_err();
            assert_eq!(e.kind(), ErrorKind::Connect, "{old}");
            assert!(e.to_string().contains("MariaDB 10.6 and later"), "{e}");
        }
        assert_eq!(Server::MariaDb.handshake_collation(), 45);
        assert_eq!(Server::MySql.handshake_collation(), 255);
    }

    #[test]
    fn a_large_message_does_not_leave_a_large_buffer_behind() {
        let mut buf = ReadBuf::new();
        buf.extend(&packet(1, &vec![7; 2 << 20]));
        let Some(Message::Here { payload, .. }) = buf.next(usize::MAX) else {
            panic!()
        };
        buf.consume(&payload);
        assert!(buf.data.capacity() > crate::client::KEEP_CAPACITY);
        buf.extend(&packet(2, b"small"));
        assert!(buf.data.capacity() < 64 * 1024, "{}", buf.data.capacity());
        let Some(Message::Here { seq, payload }) = buf.next(100) else {
            panic!()
        };
        assert_eq!((seq, buf.bytes(payload)), (2, &b"small"[..]));
    }

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

    #[test]
    fn a_receive_into_lent_storage_reads_what_copying_reads() {
        let big = vec![7u8; MAX_PAYLOAD + 5];
        let mut out = packet(1, b"hello");
        let start = protocol::begin(&mut out);
        out.extend_from_slice(&big);
        protocol::finish(&mut out, start, 3);
        out.extend(packet(5, b"next"));
        for room in [1, 7, 4096, 64 * 1024] {
            // Over the limit, so the long message is thrown away as it lands.
            let mut buf = ReadBuf::new();
            let mut seen = Vec::new();
            for chunk in out.chunks(room.max(1000)) {
                feed_lent(&mut buf, chunk, room);
                while let Some(m) = buf.next(1000) {
                    match m {
                        Message::Here { seq, payload } => {
                            seen.push((seq, buf.bytes(payload.clone()).to_vec()));
                            buf.consume(&payload);
                        }
                        Message::TooLarge { seq, next } => seen.push((seq + next, Vec::new())),
                        Message::Joined { .. } | Message::OutOfOrder { .. } => {
                            panic!("room {room}")
                        }
                    }
                }
                assert!(
                    buf.data.len() < 1 << 20,
                    "room {room}: the big message was kept"
                );
            }
            assert_eq!(
                seen,
                [
                    (1, b"hello".to_vec()),
                    (8, Vec::new()),
                    (5, b"next".to_vec())
                ],
                "room {room}"
            );
        }
    }

    fn packet(seq: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        protocol::packet(&mut out, seq, |o| o.extend_from_slice(payload));
        out
    }

    #[test]
    fn messages_are_found_across_chunks() {
        let mut buf = ReadBuf::new();
        let mut bytes = packet(1, b"hello");
        bytes.extend(packet(2, b"world"));
        buf.extend(&bytes[..3]);
        assert!(buf.next(100).is_none());
        buf.extend(&bytes[3..12]);
        let Some(Message::Here { seq, payload }) = buf.next(100) else {
            panic!()
        };
        assert_eq!((seq, buf.bytes(payload.clone())), (1, &b"hello"[..]));
        buf.consume(&payload);
        assert!(buf.next(100).is_none());
        buf.extend(&bytes[12..]);
        let Some(Message::Here { seq, payload }) = buf.next(100) else {
            panic!()
        };
        assert_eq!((seq, buf.bytes(payload.clone())), (2, &b"world"[..]));
        buf.consume(&payload);
    }

    #[test]
    fn long_messages_are_joined_or_thrown_away() {
        let big = vec![7u8; MAX_PAYLOAD + 5];
        let mut out = Vec::new();
        let start = protocol::begin(&mut out);
        out.extend_from_slice(&big);
        protocol::finish(&mut out, start, 3);
        out.extend(packet(5, b"next"));

        let mut buf = ReadBuf::new();
        for chunk in out.chunks(1 << 20) {
            buf.extend(chunk);
        }
        let Some(Message::Joined { seq, next, payload }) = buf.next(usize::MAX) else {
            panic!()
        };
        assert_eq!((seq, next), (3, 5), "two packets");
        assert_eq!(payload, big);
        assert!(matches!(buf.next(100), Some(Message::Here { seq: 5, .. })));

        // Over the limit: thrown away as it arrives, and what follows is
        // still found.
        let mut buf = ReadBuf::new();
        let mut reported = 0;
        let mut next = None;
        for chunk in out.chunks(1 << 20) {
            buf.extend(chunk);
            while let Some(m) = buf.next(1000) {
                match m {
                    Message::TooLarge { seq, next } => {
                        assert_eq!((seq, next), (3, 5));
                        reported += 1;
                    }
                    Message::Here { seq, payload } => {
                        next = Some((seq, buf.bytes(payload.clone()).to_vec()));
                        buf.consume(&payload);
                    }
                    Message::Joined { .. } | Message::OutOfOrder { .. } => {
                        panic!("joined past the limit")
                    }
                }
            }
        }
        assert_eq!(reported, 1);
        assert_eq!(next, Some((5, b"next".to_vec())));
        assert!(buf.data.len() < 1 << 21, "the big message was buffered");

        // The second packet of a long message must follow the first.
        let mut wrong = out.clone();
        wrong[4 + MAX_PAYLOAD + 3] = 9;
        for limit in [usize::MAX, 1000] {
            let mut buf = ReadBuf::new();
            buf.extend(&wrong);
            assert!(
                matches!(
                    buf.next(limit),
                    Some(Message::OutOfOrder {
                        expected: 4,
                        got: 9
                    })
                ),
                "limit {limit}"
            );
        }
    }
}
