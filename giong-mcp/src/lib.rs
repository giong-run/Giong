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
//!     // On when GIONG_MCP is set, for example GIONG_MCP=127.0.0.1:7777.
//!     let _mcp = giong_mcp::start_from_env(&rt.handle())?;
//!     rt.run(|_worker| async {
//!         // the service
//!         giong::shutdown_signal().await;
//!     })
//! }
//! ```
//!
//! Then point the agent at it, for Claude Code:
//! `claude mcp add --transport http giong http://127.0.0.1:7777/mcp`.
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
//! MCP's Streamable HTTP transport, at `/mcp`: one JSON-RPC message per
//! `POST`, answered with one JSON response (no event streams). Requests
//! whose `Host` is not a loopback name, or that carry an `Origin` that is
//! not one, are refused, which stops web pages from reaching the server
//! through DNS rebinding. With a token ([`Config::token`],
//! `GIONG_MCP_TOKEN`), every request needs `Authorization: Bearer <token>`.

#![warn(missing_docs)]

mod http;
mod proc;
mod rpc;
mod tools;
mod watch;

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
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
    snapshot_timeout: Duration,
}

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

    /// Listens on a Unix socket at `path`. A stale socket file left by a
    /// process that is gone is replaced; a live one is not.
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self::with(Addr::Unix(path.into()))
    }

    fn with(addr: Addr) -> Self {
        Self {
            addr,
            token: None,
            snapshot_timeout: Duration::from_millis(500),
        }
    }

    /// The configuration in the environment, or `None` when `GIONG_MCP` is
    /// unset or empty: `GIONG_MCP` is the address (`127.0.0.1:7777`, or
    /// `unix:` and a path), and `GIONG_MCP_TOKEN`, when set and not empty,
    /// the token.
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
        if let Ok(token) = std::env::var("GIONG_MCP_TOKEN") {
            if !token.is_empty() {
                cfg = cfg.token(token);
            }
        }
        Ok(Some(cfg))
    }

    /// Requires `Authorization: Bearer <token>` on every request. Without a
    /// token, any process on the machine that can reach the address can
    /// read what the server shows; a Unix socket's file permissions are
    /// another way to limit that.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// How long the server waits for a worker to describe itself before
    /// reporting it as not answering (default: 500 ms). A worker blocked in
    /// a task cannot answer; the server then reports which task it is in.
    pub fn snapshot_timeout(mut self, timeout: Duration) -> Self {
        self.snapshot_timeout = timeout.max(Duration::from_millis(1));
        self
    }

    /// Starts the server on a thread of its own, looking at the runtime
    /// `runtime` belongs to. Returns once it is listening; dropping the
    /// returned [`McpServer`] stops it.
    pub fn start(self, runtime: &Handle) -> io::Result<McpServer> {
        let target = runtime.clone();
        let listen = match &self.addr {
            Addr::Tcp(addr) => Listen::Tcp(parse_local(addr)?),
            Addr::Unix(path) => {
                clear_stale_socket(path)?;
                Listen::Unix(path.clone())
            }
        };
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
            .field("snapshot_timeout", &self.snapshot_timeout)
            .finish()
    }
}

/// Starts the server described by the environment (see
/// [`Config::from_env`]), and writes its URL to stderr. Returns `None`, and
/// starts nothing, when `GIONG_MCP` is unset or empty.
pub fn start_from_env(runtime: &Handle) -> io::Result<Option<McpServer>> {
    let Some(cfg) = Config::from_env()? else {
        return Ok(None);
    };
    let server = cfg.start(runtime)?;
    eprintln!("giong-mcp: listening on {}", server.url());
    Ok(Some(server))
}

/// A running MCP server. Dropping it stops the server and waits for its
/// thread.
pub struct McpServer {
    handle: Handle,
    thread: Option<thread::JoinHandle<()>>,
    bound: Bound,
}

#[derive(Clone, Debug)]
enum Bound {
    Tcp(SocketAddr),
    Unix(PathBuf),
}

impl McpServer {
    /// Where to point an MCP client: `http://127.0.0.1:7777/mcp`, or
    /// `unix:/path/to/socket` followed by `/mcp` for a Unix socket.
    pub fn url(&self) -> String {
        match &self.bound {
            Bound::Tcp(addr) => format!("http://{addr}/mcp"),
            Bound::Unix(path) => format!("unix:{}/mcp", path.display()),
        }
    }

    /// The TCP address it listens on; `None` for a Unix socket.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match self.bound {
            Bound::Tcp(addr) => Some(addr),
            Bound::Unix(_) => None,
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
            if let Bound::Unix(path) = &self.bound {
                let _ = std::fs::remove_file(path);
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
                let listener = TcpListener::bind(addr)?;
                let addr = listener.local_addr()?;
                Ok((Listener::Tcp(listener), Bound::Tcp(addr)))
            }
            Listen::Unix(path) => {
                let listener = UnixListener::bind(&path)?;
                Ok((Listener::Unix(listener), Bound::Unix(path)))
            }
        }
    }
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

/// Removes a socket file nobody listens on any more, so that a restarted
/// service can bind again. Anything else at that path is left alone, and
/// binding then fails with the kernel's error.
fn clear_stale_socket(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !meta.file_type().is_socket() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => std::fs::remove_file(path),
        _ => Ok(()),
    }
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
}
