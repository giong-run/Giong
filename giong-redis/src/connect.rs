//! Opening a connection: the socket, TLS, and the RESP3 handshake.

use std::io;
use std::net::Shutdown;
use std::ops::Deref;
#[cfg(unix)]
use std::os::fd::{AsRawFd, BorrowedFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, BorrowedSocket};
use std::time::Duration;

use giong::net::{TcpStream, UnixStream};
use giong::RecvBuf;
use giong_tls::TlsStream;
use socket2::{SockRef, TcpKeepalive};

use crate::cmd::{cmd, Cmd};
use crate::config::{Config, Host};
use crate::error::{Error, ErrorKind, ServerError};
use crate::resp::{Next, ReadBuf};
use crate::value::Value;

/// A dead peer is noticed after about a minute: keepalive probes start after
/// 30 s of silence and give up after three more 10 s apart, and data the
/// server has not acknowledged for 60 s fails the connection.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_RETRIES: u32 = 3;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const USER_TIMEOUT: Duration = Duration::from_secs(60);

/// The largest reply accepted during the handshake.
const SETUP_LIMIT: usize = 1 << 20;

/// Free room asked of the read buffer before each receive: replies that
/// arrive together are taken in one receive up to this much.
const READ_ROOM: usize = 64 * 1024;

/// The socket under a connection.
pub(crate) enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
    Tls(Box<TlsStream>),
    /// No socket: for driving a connection's reply matching from the fuzz
    /// target and the tests, without a runtime.
    #[cfg(any(test, fuzzing))]
    Detached,
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
            Stream::Detached => std::future::pending().await,
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
            Stream::Detached => (Ok(()), buf),
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
            Stream::Detached => Ok(()),
        };
    }
}

/// What the server said while the connection was being set up.
pub(crate) struct Startup {
    pub stream: Stream,
    pub buf: ReadBuf,
    pub server: Server,
}

/// What `HELLO` told about the server.
#[derive(Clone, Debug, Default)]
pub(crate) struct Server {
    /// `redis` or `valkey`.
    pub name: String,
    pub version: String,
    pub id: i64,
    /// `standalone`, `sentinel` or `cluster`.
    pub mode: String,
}

fn connect_error(config: &Config, message: impl std::fmt::Display, e: io::Error) -> Error {
    Error::io(
        ErrorKind::Connect,
        format!("cannot connect to {}: {message}", config.target()),
        e,
    )
}

/// Turns off Nagle's algorithm (commands are written whole) and sets up
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
    // Only Linux has TCP_USER_TIMEOUT; elsewhere the keepalive probes alone
    // notice a vanished server.
    #[cfg(target_os = "linux")]
    sock.set_tcp_user_timeout(Some(USER_TIMEOUT))?;
    Ok(())
}

/// Opens a connection and logs in, within the connect timeout.
pub(crate) async fn connect(config: &Config) -> Result<Startup, Error> {
    match giong::time::timeout(config.connect_timeout, open(config)).await {
        Ok(result) => result,
        Err(_) => Err(Error::new(
            ErrorKind::Connect,
            format!(
                "cannot connect to {}: no answer within the connect timeout of {:?} (raise it \
                 with Config::connect_timeout, or check that the server is reachable)",
                config.target(),
                config.connect_timeout
            ),
        )),
    }
}

async fn open(config: &Config) -> Result<Startup, Error> {
    let stream = match &config.host {
        Host::Unix(path) => {
            let stream = UnixStream::connect(path).await.map_err(|e| {
                connect_error(
                    config,
                    "is the server running, and is this its socket (its `unixsocket` setting)?",
                    e,
                )
            })?;
            Stream::Unix(stream)
        }
        Host::Tcp(host) => {
            let tls = config.tls_connector()?;
            let tcp = TcpStream::connect((host.as_str(), config.port))
                .await
                .map_err(|e| connect_error(config, "is the server running and reachable?", e))?;
            tune(&tcp).map_err(|e| connect_error(config, "cannot set socket options", e))?;
            match tls {
                None => Stream::Tcp(tcp),
                Some(connector) => {
                    let tls = connector.connect(host, tcp).await.map_err(|e| {
                        connect_error(
                            config,
                            "the TLS handshake failed (does the server listen for TLS on this \
                             port, its `tls-port`?)",
                            e,
                        )
                    })?;
                    Stream::Tls(Box::new(tls))
                }
            }
        }
    };
    handshake(config, stream).await
}

/// `HELLO 3` with the credentials and the client name, then `SELECT`, sent
/// together.
async fn handshake(config: &Config, stream: Stream) -> Result<Startup, Error> {
    let mut hello = cmd("HELLO").arg(3);
    if let Some(password) = &config.password {
        hello = hello
            .arg("AUTH")
            .arg(config.user.as_deref().unwrap_or("default"))
            .arg(password.as_str());
    } else if config.user.is_some() {
        return Err(Error::config(
            "a user name was given without a password; Redis logs in with both",
        ));
    }
    if let Some(name) = &config.client_name {
        if name.is_empty() || name.bytes().any(|b| b <= b' ' || b > b'~') {
            return Err(Error::config(format!(
                "the client name {name:?} must be printable ASCII without spaces"
            )));
        }
        hello = hello.arg("SETNAME").arg(name.as_str());
    }
    let mut out = Vec::new();
    hello.encode(&mut out);
    let select: Option<Cmd> = (config.database != 0).then(|| cmd("SELECT").arg(config.database));
    if let Some(s) = &select {
        s.encode(&mut out);
    }
    let (written, _) = stream.write_all(out).await;
    written.map_err(|e| connect_error(config, "writing to the server failed", e))?;

    let mut buf = ReadBuf::new();
    let reply = read(config, &stream, &mut buf).await?;
    let server = match reply {
        Value::Map(fields) => server_info(fields),
        Value::Error(e) => {
            // The SELECT after it was refused too (not logged in); read it
            // so the message is about the first error only.
            return Err(hello_error(config, e));
        }
        other => {
            return Err(Error::protocol(format!(
                "the server answered HELLO with {}",
                other.describe()
            )))
        }
    };
    if select.is_some() {
        match read(config, &stream, &mut buf).await? {
            Value::Status(_) => {}
            Value::Error(e) => {
                return Err(Error::with_server(
                    ErrorKind::Connect,
                    format!(
                        "the server at {} refused database {} (it has `databases` of them, 16 \
                         unless set otherwise; a cluster has only database 0)",
                        config.target(),
                        config.database
                    ),
                    e,
                ))
            }
            other => {
                return Err(Error::protocol(format!(
                    "the server answered SELECT with {}",
                    other.describe()
                )))
            }
        }
    }
    Ok(Startup {
        stream,
        buf,
        server,
    })
}

/// Reads one reply during the handshake.
async fn read(config: &Config, stream: &Stream, buf: &mut ReadBuf) -> Result<Value, Error> {
    loop {
        match buf.next(SETUP_LIMIT)? {
            Next::Ready(n) => {
                if buf.is_push() {
                    buf.skip(n);
                    continue;
                }
                return buf.take(n);
            }
            // A push message nobody waits for: passed over.
            Next::TooLarge { push: true } => continue,
            Next::TooLarge { push: false } => {
                return Err(Error::protocol(
                    "the server sent a reply of more than 1 MiB to the handshake",
                ))
            }
            Next::Pending => {}
        }
        let n = stream
            .read_into(buf)
            .await
            .map_err(|e| connect_error(config, "reading from the server failed", e))?;
        if n == 0 {
            return Err(Error::new(
                ErrorKind::Connect,
                format!(
                    "cannot connect to {}: the server closed the connection during the \
                     handshake (too many clients, or protected mode refusing a remote client \
                     without a password?)",
                    config.target()
                ),
            ));
        }
    }
}

/// Turns the server's refusal of `HELLO` into an error that says what to do.
fn hello_error(config: &Config, e: ServerError) -> Error {
    let target = config.target();
    match e.code() {
        "WRONGPASS" => Error::with_server(
            ErrorKind::Auth,
            format!("the server at {target} refused the user name or password"),
            e,
        ),
        "NOAUTH" => Error::with_server(
            ErrorKind::Auth,
            format!(
                "the server at {target} needs a password: set Config::password, or \
                 :password@ in the URL"
            ),
            e,
        ),
        "NOPERM" => Error::with_server(
            ErrorKind::Auth,
            format!("the user may not run HELLO on the server at {target}"),
            e,
        ),
        "NOPROTO" => Error::with_server(
            ErrorKind::Connect,
            format!("the server at {target} does not speak RESP3, which giong-redis needs"),
            e,
        ),
        _ if e.message().contains("unknown command") => Error::with_server(
            ErrorKind::Connect,
            format!(
                "the server at {target} does not know HELLO: giong-redis needs Redis 6 or \
                 later, or Valkey"
            ),
            e,
        ),
        _ => Error::with_server(
            ErrorKind::Connect,
            format!("the server at {target} refused the handshake"),
            e,
        ),
    }
}

fn server_info(fields: Vec<(Value, Value)>) -> Server {
    let mut server = Server::default();
    for (k, v) in fields {
        let text = || v.as_str().unwrap_or_default().to_owned();
        match k.as_str() {
            Some("server") => server.name = text(),
            Some("version") => server.version = text(),
            Some("mode") => server.mode = text(),
            Some("id") => server.id = v.as_int().unwrap_or(0),
            _ => {}
        }
    }
    server
}
