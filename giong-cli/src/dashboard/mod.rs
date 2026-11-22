//! The local dashboard: one page in the browser, served by the `giong`
//! command (never by the service), showing the service `giong dev` runs here.
//!
//! - Build state, restarts and listen addresses: `.giong/dev.json`.
//! - The live request log, errors and recorded exchanges: the service's
//!   output, which `giong dev` keeps in `.giong/dev.log` (giong-web's
//!   `Logger` and `Record` lines are JSON).
//! - Routes, workers, tasks, connection pools and warnings: the service's
//!   giong-mcp, as `giong routes` and `giong top` read it.
//! - "Send a request": the service's own port, through [`crate::web_client`].
//!
//! Nothing is added to the service: it pays nothing for the dashboard.
//!
//! The server listens on 127.0.0.1 only. It answers only requests whose
//! `Host` is that loopback address (so a web page cannot reach it by DNS
//! rebinding), and its API needs an `X-Giong-Dashboard` header, which a
//! page of another origin cannot send without a CORS preflight the server
//! refuses: other sites cannot read the dashboard or send requests through
//! it.

pub mod api;
pub mod http;

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

/// The dashboard's usual port.
pub const PORT: u16 = 7700;

/// How many connections are served at once; more wait for the listener.
const MAX_CONNECTIONS: usize = 32;

/// What the dashboard shows: where the project is and how to reach its
/// service.
#[derive(Clone, Debug, Default)]
pub struct Target {
    /// The project root (`.giong/dev.json` and `.giong/dev.log` under it).
    pub root: Option<PathBuf>,
    /// `--mcp`, when given.
    pub mcp: Option<String>,
    /// `--base-url`, when given.
    pub base_url: Option<String>,
    /// A log to read when `giong dev` is not running (`--log`).
    pub log: Option<PathBuf>,
}

/// A bound dashboard, not serving yet.
pub struct Dashboard {
    listener: TcpListener,
    addr: SocketAddr,
    target: Arc<Target>,
}

impl Dashboard {
    /// Binds 127.0.0.1:`port` (0 for any free port).
    pub fn bind(port: u16, target: Target) -> io::Result<Dashboard> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let addr = listener.local_addr()?;
        Ok(Dashboard {
            listener,
            addr,
            target: Arc::new(target),
        })
    }

    /// Binds the first free port of `port..port + tries`.
    pub fn bind_near(port: u16, tries: u16, target: Target) -> io::Result<Dashboard> {
        let mut last = None;
        for p in port..port.saturating_add(tries.max(1)) {
            match Dashboard::bind(p, target.clone()) {
                Ok(d) => return Ok(d),
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| io::Error::from(io::ErrorKind::AddrInUse)))
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The address to open in a browser.
    pub fn url(&self) -> String {
        format!("http://{}/", self.addr)
    }

    /// Serves until the process ends.
    pub fn serve(self) {
        let active = Arc::new(AtomicUsize::new(0));
        for stream in self.listener.incoming() {
            let Ok(stream) = stream else { continue };
            // Never more than MAX_CONNECTIONS threads: a busy browser waits.
            while active.load(Ordering::Acquire) >= MAX_CONNECTIONS {
                thread::sleep(std::time::Duration::from_millis(5));
            }
            active.fetch_add(1, Ordering::AcqRel);
            let target = self.target.clone();
            let counter = active.clone();
            let port = self.addr.port();
            let spawned = thread::Builder::new()
                .name("giong-dashboard".into())
                .spawn(move || {
                    http::serve_connection(stream, port, &|req| api::handle(req, &target));
                    counter.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                // No thread: the closure, and the connection, were dropped.
                active.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

/// Starts the dashboard for `giong dev --ui` on a background thread: binds
/// `port` or one of the nine after it, and gives the address bound.
pub fn spawn(root: PathBuf, port: u16) -> io::Result<SocketAddr> {
    let d = Dashboard::bind_near(
        port,
        10,
        Target {
            root: Some(root),
            ..Target::default()
        },
    )?;
    let addr = d.local_addr();
    thread::Builder::new()
        .name("giong-dashboard".into())
        .spawn(move || d.serve())?;
    Ok(addr)
}
