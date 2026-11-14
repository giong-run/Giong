//! # giong-mcp
//!
//! An [MCP](https://modelcontextprotocol.io) server that lets an AI agent
//! look inside a running giong service: what each worker is doing, which
//! tasks are alive and where they were spawned, what the connection pools
//! hold, and where the time goes.
//!
//! It is read-only, listens only on a loopback address or a Unix socket,
//! and runs only when the service turns it on. It runs on a thread and a
//! giong runtime of its own, so it keeps answering when every worker of the
//! service is busy or stuck, and says which task a stuck worker is in.
//!
//! ```no_run
//! fn main() -> std::io::Result<()> {
//!     let rt = giong::Builder::new().build()?;
//!     // On when GIONG_MCP is set, for example GIONG_MCP=127.0.0.1:7777 with
//!     // GIONG_MCP_TOKEN=$(openssl rand -hex 16). Keep the value: dropping it
//!     // stops the server.
//!     let _mcp = giong_mcp::start_from_env(&rt.handle());
//!     rt.run(|_worker| async {
//!         // the service
//!         giong::shutdown_signal().await;
//!     })
//! }
//! ```
//!
//! Then point the agent at it, for Claude Code:
//! `claude mcp add --transport http giong http://127.0.0.1:7777/mcp
//! --header "Authorization: Bearer $GIONG_MCP_TOKEN"`.
//!
//! ## Tools
//!
//! | Tool | What it answers |
//! |---|---|
//! | `overview` | the process (memory, threads, descriptors, uptime), each worker's state, driver and load, and warnings such as a blocked worker |
//! | `workers` | each worker in detail: rates over the last second, how busy it is, timers, I/O in flight, receive buffers, and the task it is blocked in |
//! | `tasks` | live tasks grouped by the line that spawned them, per worker |
//! | `sources` | what registered sources report: giong-postgres, giong-mysql and giong-redis pools |
//! | `profile` | times every task poll for a while and says which spawn sites the workers spent their time in |
//!
//! Each answer is JSON, as `structuredContent` and as text.
//!
//! ## Transport and security
//!
//! MCP's Streamable HTTP transport over HTTP/1.1, at `/mcp`: one JSON-RPC
//! message per `POST`, answered with one JSON response (no event streams).
//! Requests whose `Host` is not a loopback name, or that carry an `Origin`
//! that is not one, are refused, which stops web pages from reaching the
//! server through DNS rebinding.
//!
//! On TCP, every request needs `Authorization: Bearer <token>`
//! ([`Config::token`], `GIONG_MCP_TOKEN`): any local process can connect to
//! a loopback port, so a TCP server without a token does not start
//! ([`Code::McpNoToken`]) unless the code says it should
//! ([`Config::allow_unauthenticated`]). A Unix socket is created readable
//! and writable by the service's user only, and needs no token.
//!
//! What one request can cost is bounded, so that a client cannot make the
//! service's process run out of memory: a body of at most 64 KiB, a batch
//! of at most 16 messages (and none from clients of MCP 2025-06-18 and
//! later, which removed batches), an answer of at most 4 MiB, and 16
//! connections, each answered one request at a time.

#![warn(missing_docs)]

mod http;
mod proc;
mod rpc;
mod schema;
mod tools;
mod watch;

/// Entry points for the fuzz target in `giong-mcp/fuzz`, built only for it
/// (`cargo fuzz` sets `--cfg fuzzing`) and the tests. Not part of the API.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzzing;

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use giong::error::{Code, Error};
use giong::net::{TcpListener, UnixListener};
use giong::Handle;

/// Where the server listens, and how.
#[derive(Clone)]
pub struct Config {
    addr: Addr,
    token: Option<String>,
    unauthenticated: bool,
    snapshot_timeout: Duration,
}

/// The longest [`Config::snapshot_timeout`].
const MAX_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
/// The shortest token [`Config::token`] accepts.
const MIN_TOKEN_LEN: usize = 16;

#[derive(Clone, Debug)]
enum Addr {
    /// Not parsed yet: errors are reported by `start`.
    Tcp(String),
    Unix(PathBuf),
}

impl Config {
    /// Listens on a loopback TCP address: `127.0.0.1:7777`, `[::1]:7777` or
    /// `localhost:7777`. Port 0 picks a free port ([`McpServer::url`] says
    /// which). Any other address is refused when the server starts
    /// ([`Code::McpNotLocal`]).
    pub fn tcp(addr: impl Into<String>) -> Self {
        Self::with(Addr::Tcp(addr.into()))
    }

    /// Listens on a Unix socket at `path`, created readable and writable by
    /// the process's user only (mode `0600`), so that it needs no token. A
    /// stale socket file left by a process that is gone is replaced; a live
    /// one is not ([`Code::McpBind`]).
    ///
    /// Most MCP clients speak HTTP over TCP only; to reach the socket from
    /// one, bridge it to a loopback port with a token-less forwarder the
    /// same user runs, for example `socat
    /// TCP-LISTEN:7777,bind=127.0.0.1,fork UNIX-CONNECT:/run/giong/mcp.sock`
    /// (which lets every local process in, as
    /// [`allow_unauthenticated`](Self::allow_unauthenticated) does), or
    /// `ssh -L 7777:/run/giong/mcp.sock host` from another machine.
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self::with(Addr::Unix(path.into()))
    }

    fn with(addr: Addr) -> Self {
        Self {
            addr,
            token: None,
            unauthenticated: false,
            snapshot_timeout: Duration::from_millis(500),
        }
    }

    /// The configuration in the environment, or `None` when `GIONG_MCP` is
    /// unset or empty: `GIONG_MCP` is the address (`127.0.0.1:7777`, or
    /// `unix:` and a path), and `GIONG_MCP_TOKEN`, when set, the token. A
    /// `GIONG_MCP_TOKEN` that is set but empty is a mistake that
    /// [`start`](Self::start) reports ([`Code::McpBadToken`]), not a way to
    /// turn the token off.
    pub fn from_env() -> io::Result<Option<Self>> {
        let addr = match std::env::var("GIONG_MCP") {
            Ok(v) if !v.trim().is_empty() => v.trim().to_owned(),
            Ok(_) | Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(bad_address("GIONG_MCP is not valid UTF-8".into()))
            }
        };
        let mut cfg = match addr.strip_prefix("unix:") {
            Some(path) => Self::unix(path),
            None => Self::tcp(addr),
        };
        match std::env::var("GIONG_MCP_TOKEN") {
            Ok(token) => cfg = cfg.token(token),
            Err(std::env::VarError::NotPresent) => {}
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(bad_token("GIONG_MCP_TOKEN is not valid UTF-8".into()))
            }
        }
        Ok(Some(cfg))
    }

    /// Requires `Authorization: Bearer <token>` on every request. The token
    /// must be at least 16 characters of the kinds a bearer token can carry
    /// (letters, digits and `-._~+/`, `=` at the end), or
    /// [`start`](Self::start) fails with [`Code::McpBadToken`]; `openssl
    /// rand -hex 16` makes one.
    ///
    /// On TCP a token is required: any process on the machine can connect
    /// to a loopback port. On a Unix socket it is optional, since the
    /// socket's file permissions already keep other users out.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Serves requests without a token on TCP: every process on the
    /// machine, whichever user runs it, and every container sharing the
    /// network namespace, can then read what the server shows and run its
    /// profiler. For a development machine; without it, a TCP server with
    /// no [`token`](Self::token) does not start ([`Code::McpNoToken`]). A
    /// token, when one is given, is still required.
    pub fn allow_unauthenticated(mut self) -> Self {
        self.unauthenticated = true;
        self
    }

    /// How long the server waits for a worker to describe itself before
    /// reporting it as not answering (default: 500 ms, at least 1 ms, at
    /// most 10 s). A worker blocked in a task cannot answer; the server then
    /// reports which task it is in.
    pub fn snapshot_timeout(mut self, timeout: Duration) -> Self {
        self.snapshot_timeout = timeout.clamp(Duration::from_millis(1), MAX_SNAPSHOT_TIMEOUT);
        self
    }

    /// Starts the server on a thread of its own, looking at the runtime
    /// `runtime` belongs to. Returns once it is listening; dropping the
    /// returned [`McpServer`] stops it.
    ///
    /// # Errors
    ///
    /// [`Code::McpBadAddress`] and [`Code::McpNotLocal`] for the address,
    /// [`Code::McpNoToken`] and [`Code::McpBadToken`] for the token, and
    /// [`Code::McpBind`] when the address cannot be listened on.
    pub fn start(self, runtime: &Handle) -> io::Result<McpServer> {
        let target = runtime.clone();
        let listen = match &self.addr {
            Addr::Tcp(addr) => Listen::Tcp(parse_local(addr)?),
            Addr::Unix(path) => Listen::Unix(path.clone()),
        };
        if let Some(token) = &self.token {
            check_token(token)?;
        } else if matches!(listen, Listen::Tcp(_)) && !self.unauthenticated {
            return Err(Error::new(
                Code::McpNoToken,
                "giong-mcp will not serve TCP without a token: every local process could read \
                 the service's internals",
                format!(
                    "set GIONG_MCP_TOKEN (or call Config::token) to a secret of at least \
                     {MIN_TOKEN_LEN} characters, such as the output of `openssl rand -hex 16`, \
                     and give clients `Authorization: Bearer <token>`; or listen on a Unix \
                     socket; or, on a trusted machine, call Config::allow_unauthenticated."
                ),
            )
            .into());
        }
        let (tx, rx) = mpsc::channel::<io::Result<(Handle, Bound)>>();
        let settings = http::Settings {
            token: self.token.clone(),
            check_host: matches!(listen, Listen::Tcp(_)),
            snapshot_timeout: self.snapshot_timeout,
        };
        let thread = thread::Builder::new()
            .name("giong-mcp".into())
            .spawn(move || {
                let fail = tx.clone();
                let ran = giong::Builder::new()
                    .ring_entries(64)
                    .recv_buffers(32, 16 * 1024)
                    .block_on(async move {
                        let bound = match listen.bind() {
                            Ok(bound) => bound,
                            Err(e) => {
                                let _ = tx.send(Err(e));
                                return;
                            }
                        };
                        let (listener, info) = bound;
                        let _ = tx.send(Ok((giong::handle(), info)));
                        drop(tx);
                        http::serve(listener, target, settings).await;
                    });
                if let Err(e) = ran {
                    let _ = fail.send(Err(e));
                }
            })?;
        match rx.recv() {
            Ok(Ok((handle, bound))) => Ok(McpServer {
                handle,
                thread: Some(thread),
                bound,
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(io::Error::other(
                    "giong-mcp: the server thread stopped before listening",
                ))
            }
        }
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("addr", &self.addr)
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .field("allow_unauthenticated", &self.unauthenticated)
            .field("snapshot_timeout", &self.snapshot_timeout)
            .field("max_connections", &http::MAX_CONNECTIONS)
            .field("max_body_bytes", &http::MAX_BODY)
            .field("max_batch", &rpc::MAX_BATCH)
            .field("max_answer_bytes", &rpc::MAX_ANSWER)
            .finish()
    }
}

/// Starts the server described by the environment (see
/// [`Config::from_env`]) and says on stderr what happened: one line with
/// its URL, or with the error that kept it from starting. Starts nothing
/// when `GIONG_MCP` is unset or empty.
///
/// It never stops the service: the server is a debugging aid, so a busy
/// port or a bad `GIONG_MCP` leaves the service running without it, and the
/// error stays in the returned value. Keep that value for as long as the
/// server should run (`let _mcp = ...`); dropping it stops the server.
///
/// To write the line yourself, as JSON for example, or to stop the service
/// when the server cannot start, use [`Config::from_env`] and
/// [`Config::start`] instead.
pub fn start_from_env(runtime: &Handle) -> FromEnv {
    let started = match Config::from_env() {
        Ok(None) => return FromEnv::Off,
        Ok(Some(cfg)) => cfg.start(runtime),
        Err(e) => Err(e),
    };
    match started {
        Ok(server) => {
            eprintln!("giong-mcp: listening on {}", server.url());
            FromEnv::Running(server)
        }
        Err(e) => {
            eprintln!("giong-mcp: not started, the service runs without it: {e}");
            FromEnv::Failed(e)
        }
    }
}

/// What [`start_from_env`] did. Dropping it stops the server, if one runs.
///
/// So bind it to a name that lives as long as the service, `let _mcp =
/// start_from_env(&handle);` (not `let _ = ...`, which drops it at once).
/// The statement form is a warning:
///
/// ```compile_fail
/// # #![deny(unused_must_use)]
/// # fn service(handle: &giong::Handle) {
/// giong_mcp::start_from_env(handle); // the server would stop right here
/// # }
/// ```
#[must_use = "dropping it stops the MCP server at once: keep it, `let _mcp = start_from_env(..);`"]
#[derive(Debug)]
pub enum FromEnv {
    /// `GIONG_MCP` is unset or empty: nothing was started.
    Off,
    /// The server is listening.
    Running(McpServer),
    /// `GIONG_MCP` is set, but the server could not start (a coded error
    /// such as [`Code::McpBind`]); the service runs without it. The error
    /// was written to stderr.
    Failed(io::Error),
}

impl FromEnv {
    /// The running server, if one was started.
    pub fn server(&self) -> Option<&McpServer> {
        match self {
            FromEnv::Running(server) => Some(server),
            _ => None,
        }
    }

    /// Why the server did not start, if `GIONG_MCP` asked for one.
    pub fn error(&self) -> Option<&io::Error> {
        match self {
            FromEnv::Failed(e) => Some(e),
            _ => None,
        }
    }
}

/// A running MCP server. Dropping it stops the server and waits for its
/// thread, so dropping it at once is a warning:
///
/// ```compile_fail
/// # #![deny(unused_must_use)]
/// # fn service(handle: &giong::Handle) -> std::io::Result<()> {
/// giong_mcp::Config::unix("/run/giong/mcp.sock").start(handle)?; // stopped at once
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping McpServer stops the server at once: keep it, `let _mcp = ...;`"]
pub struct McpServer {
    handle: Handle,
    thread: Option<thread::JoinHandle<()>>,
    bound: Bound,
}

#[derive(Clone, Debug)]
enum Bound {
    Tcp(SocketAddr),
    /// The path, and the socket file this server created there.
    Unix(PathBuf, FileId),
}

/// Tells one file from another at the same path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    dev: u64,
    ino: u64,
}

impl FileId {
    fn of(meta: &std::fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
        }
    }

    fn at(path: &Path) -> Option<Self> {
        std::fs::symlink_metadata(path).ok().map(|m| Self::of(&m))
    }
}

impl McpServer {
    /// Where to point an MCP client: `http://127.0.0.1:7777/mcp`, or
    /// `unix:/path/to/socket` followed by `/mcp` for a Unix socket (which
    /// most clients cannot use directly: see [`Config::unix`]).
    pub fn url(&self) -> String {
        match &self.bound {
            Bound::Tcp(addr) => format!("http://{addr}/mcp"),
            Bound::Unix(path, _) => format!("unix:{}/mcp", path.display()),
        }
    }

    /// The TCP address it listens on; `None` for a Unix socket.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match self.bound {
            Bound::Tcp(addr) => Some(addr),
            Bound::Unix(..) => None,
        }
    }

    /// The path of the Unix socket it listens on; `None` on TCP.
    pub fn unix_path(&self) -> Option<&Path> {
        match &self.bound {
            Bound::Tcp(_) => None,
            Bound::Unix(path, _) => Some(path),
        }
    }

    /// Stops the server and waits for its thread. Requests being answered
    /// are dropped.
    pub fn stop(mut self) {
        self.shut_down();
    }

    fn shut_down(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.handle.shutdown();
            let _ = thread.join();
            if let Bound::Unix(path, ours) = &self.bound {
                // Only the socket this server created: a later process may
                // have replaced it since.
                remove_if(path, *ours);
            }
        }
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.shut_down();
    }
}

impl fmt::Debug for McpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServer")
            .field("url", &self.url())
            .finish()
    }
}

enum Listen {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

pub(crate) enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listen {
    fn bind(self) -> io::Result<(Listener, Bound)> {
        match self {
            Listen::Tcp(addr) => {
                let listener =
                    TcpListener::bind(addr).map_err(|e| bind_error(e, &addr.to_string()))?;
                let addr = listener.local_addr()?;
                Ok((Listener::Tcp(listener), Bound::Tcp(addr)))
            }
            Listen::Unix(path) => {
                let (listener, id) = bind_unix(&path)
                    .map_err(|e| bind_error(e, &format!("unix:{}", path.display())))?;
                Ok((Listener::Unix(listener), Bound::Unix(path, id)))
            }
        }
    }
}

fn bind_error(e: io::Error, addr: &str) -> io::Error {
    // Already explained (a live socket in the way).
    if giong::error::code(&e).is_some() {
        return e;
    }
    io::Error::new(
        e.kind(),
        Error::new(
            Code::McpBind,
            format!("giong-mcp cannot listen on {addr} ({e})"),
            "give every process its own GIONG_MCP address (another port, port 0 for any free \
             one, or a socket path per process), stop whatever holds this one (`ss -ltnp` shows \
             it), or unset GIONG_MCP where the endpoint is not needed.",
        )
        .with_source(e),
    )
}

fn in_use(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::AddrInUse,
        Error::new(
            Code::McpBind,
            format!(
                "giong-mcp cannot listen on unix:{}: another process is listening there",
                path.display()
            ),
            "give every process its own socket path, or stop the process that listens there.",
        ),
    )
}

/// Binds a Unix socket at `path` that only this user can use. It is bound
/// under a temporary name in the same directory, restricted to mode `0600`,
/// and then linked to `path`, so it is never reachable with looser
/// permissions, and a live socket at `path` is never replaced: linking
/// fails if anything is there. A stale socket (nobody listening) is
/// removed first.
fn bind_unix(path: &Path) -> io::Result<(UnixListener, FileId)> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let tmp = dir.join(format!(
        ".giong-mcp-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    // A Unix socket path holds at most 107 bytes.
    if tmp.as_os_str().len() > 107 {
        return bind_in_place(path);
    }
    // Left by an earlier process with the same pid that died right here.
    let _ = std::fs::remove_file(&tmp);
    let listener = UnixListener::bind(&tmp)?;
    let linked = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
        .and_then(|()| link_over_stale(&tmp, path));
    let _ = std::fs::remove_file(&tmp);
    let id = linked?;
    Ok((listener, id))
}

fn link_over_stale(tmp: &Path, path: &Path) -> io::Result<FileId> {
    let id = FileId::at(tmp).ok_or_else(|| io::Error::other("the socket file vanished"))?;
    for _ in 0..2 {
        match std::fs::hard_link(tmp, path) {
            Ok(()) => return Ok(id),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let meta = std::fs::symlink_metadata(path)?;
                if !meta.file_type().is_socket() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!("{} exists and is not a socket", path.display()),
                    ));
                }
                if !is_stale(path) {
                    return Err(in_use(path));
                }
                remove_if(path, FileId::of(&meta));
            }
            Err(e) => return Err(e),
        }
    }
    // Replaced again since: someone else is starting on the same path.
    Err(in_use(path))
}

/// Where the temporary name would be too long for a socket address: bind
/// at `path` itself and restrict it right after.
fn bind_in_place(path: &Path) -> io::Result<(UnixListener, FileId)> {
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        if !is_stale(path) {
            return Err(in_use(path));
        }
        let _ = std::fs::remove_file(path);
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let id = FileId::at(path).ok_or_else(|| io::Error::other("the socket file vanished"))?;
    Ok((listener, id))
}

/// Nobody listens on the socket at `path`.
fn is_stale(path: &Path) -> bool {
    matches!(
        std::os::unix::net::UnixStream::connect(path),
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused
    )
}

/// Removes the file at `path` if it is still `id`.
fn remove_if(path: &Path, id: FileId) {
    if FileId::at(path) == Some(id) {
        let _ = std::fs::remove_file(path);
    }
}

fn bad_token(what: String) -> io::Error {
    Error::new(
        Code::McpBadToken,
        what,
        format!(
            "use a secret of at least {MIN_TOKEN_LEN} letters, digits or -._~+/ characters, such \
             as the output of `openssl rand -hex 16`, in GIONG_MCP_TOKEN or Config::token."
        ),
    )
    .into()
}

/// A token must be one a client can send as `Bearer <token>` (RFC 6750's
/// `b64token`) and long enough not to be guessed.
fn check_token(token: &str) -> io::Result<()> {
    if token.trim().is_empty() {
        return Err(bad_token(
            "the MCP token is empty, which would let in requests with no token at all".into(),
        ));
    }
    let body = token.trim_end_matches('=');
    let allowed = |c: char| c.is_ascii_alphanumeric() || "-._~+/".contains(c);
    if body.is_empty() || !body.chars().all(allowed) {
        return Err(bad_token(
            "the MCP token has characters a bearer token cannot carry (only letters, digits, \
             -._~+/ and a final =)"
                .into(),
        ));
    }
    if token.len() < MIN_TOKEN_LEN {
        return Err(bad_token(format!(
            "the MCP token is {} characters long, too short to keep others out",
            token.len()
        )));
    }
    Ok(())
}

fn bad_address(what: String) -> io::Error {
    Error::new(
        Code::McpBadAddress,
        what,
        "write a loopback address with a port (127.0.0.1:7777, [::1]:7777, localhost:7777) or \
         unix: and a socket path (unix:/run/giong/mcp.sock).",
    )
    .into()
}

/// Parses a loopback address. Names other than `localhost` are not
/// resolved: the point is to be sure where the server listens.
fn parse_local(addr: &str) -> io::Result<SocketAddr> {
    let parsed = match addr.parse::<SocketAddr>() {
        Ok(a) => a,
        Err(_) => {
            let port = addr
                .strip_prefix("localhost:")
                .and_then(|p| p.parse::<u16>().ok())
                .ok_or_else(|| bad_address(format!("{addr:?} is not an IP address and port")))?;
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
        }
    };
    if !parsed.ip().is_loopback() {
        return Err(Error::new(
            Code::McpNotLocal,
            format!("giong-mcp will not listen on {parsed}, which other machines can reach"),
            "listen on 127.0.0.1, [::1] or a Unix socket, and forward the port to reach it from \
             elsewhere (ssh -L, kubectl port-forward).",
        )
        .into());
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_addresses_are_accepted() {
        assert_eq!(
            parse_local("127.0.0.1:7777").unwrap(),
            "127.0.0.1:7777".parse().unwrap()
        );
        assert_eq!(parse_local("[::1]:0").unwrap(), "[::1]:0".parse().unwrap());
        assert_eq!(
            parse_local("localhost:80").unwrap(),
            "127.0.0.1:80".parse().unwrap()
        );
        for addr in ["0.0.0.0:7777", "10.1.2.3:7777", "[::]:7777"] {
            let err = parse_local(addr).unwrap_err();
            assert_eq!(giong::error::code(&err), Some(Code::McpNotLocal), "{addr}");
        }
        for addr in ["example.com:7777", "127.0.0.1", "localhost", ":7777"] {
            let err = parse_local(addr).unwrap_err();
            assert_eq!(
                giong::error::code(&err),
                Some(Code::McpBadAddress),
                "{addr}"
            );
        }
    }

    #[test]
    fn tokens_must_be_long_and_sendable() {
        for good in [
            "0123456789abcdef",
            "b2RkLXRva2VuLW9mLTI0LWJ5dGVz",
            "abc-DEF_ghi.jkl~mno+pqr/stu==",
        ] {
            check_token(good).unwrap();
        }
        for bad in [
            "",
            "   ",
            "short",
            "0123456789abcde",
            "0123456789 abcdef",
            "0123456789abcdef\n",
            "0123456789abcdef\r\nX-Evil: 1",
            "0123456789abcdé",
            "==0123456789abcdef",
            "================",
        ] {
            let err = check_token(bad).unwrap_err();
            assert_eq!(giong::error::code(&err), Some(Code::McpBadToken), "{bad:?}");
        }
    }

    #[test]
    fn snapshot_timeout_is_finite() {
        let cfg = Config::tcp("127.0.0.1:0").snapshot_timeout(Duration::MAX);
        assert_eq!(cfg.snapshot_timeout, MAX_SNAPSHOT_TIMEOUT);
        let cfg = Config::tcp("127.0.0.1:0").snapshot_timeout(Duration::ZERO);
        assert_eq!(cfg.snapshot_timeout, Duration::from_millis(1));
    }
}
