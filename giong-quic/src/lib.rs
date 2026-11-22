//! QUIC (RFC 9000) for giong: endpoints, connections and streams, the
//! transport under giong-http's HTTP/3 (`Server::serve_h3` and the HTTP/3
//! client, feature `unstable-http3`).
//!
//! One [`Endpoint`] per worker: each worker binds its own socket to the same
//! address (giong's `UdpSocket::bind` shares the port), and the kernel
//! spreads clients over the workers by their address. A connection then
//! lives on one worker for its whole life, with no lock anywhere.
//!
//! The engine underneath is quinn-proto today. A native engine built for
//! io_uring and thread-per-core (encryption in place in the receive and send
//! buffers, whole flights in one GSO send) grows in this crate and replaces
//! it behind this same API once it passes the QUIC interop runner. Nothing
//! of quinn-proto appears in this API, so the swap is invisible to users.
//!
//! ```no_run
//! # fn tls() -> rustls::ServerConfig { unimplemented!() }
//! # fn main() -> std::io::Result<()> {
//! use giong_quic::{Endpoint, ServerConfig};
//! giong::block_on(async {
//!     let ep = Endpoint::server("0.0.0.0:4433".parse().unwrap(), ServerConfig::new(tls())?)?;
//!     while let Some(conn) = ep.accept().await {
//!         giong::spawn(async move {
//!             while let Ok((mut send, mut recv)) = conn.accept_bi().await {
//!                 let req = recv.read_to_end(64 * 1024).await?;
//!                 send.write_all(&req).await?;
//!                 send.finish()?;
//!             }
//!             std::io::Result::Ok(())
//!         });
//!     }
//!     Ok(())
//! })
//! # }
//! ```

use std::io;
use std::net::SocketAddr;
use std::rc::Rc;

use bytes::Bytes;
use quinn_proto::Dir;

mod config;
mod crypto;
mod error;
/// The native engine, work in progress. Public only for its tests and fuzz
/// target: not part of the API, and outside the semver promise.
#[doc(hidden)]
pub mod native;
mod proto;
mod route;

pub use config::{ClientConfig, ServerConfig, TransportConfig};

/// A QUIC endpoint on the current giong worker: a UDP socket and the
/// connections on it. Dropping it refuses new connections; existing ones
/// carry on until they close.
pub struct Endpoint {
    sh: Rc<proto::Shared>,
}

impl Endpoint {
    /// Binds a server endpoint to `addr` on this worker. Bind one on every
    /// worker (for instance in `Runtime::run`) to use every core.
    pub fn server(addr: SocketAddr, config: ServerConfig) -> io::Result<Self> {
        let cfg = std::sync::Arc::new(config.to_proto()?);
        Ok(Self {
            sh: proto::bind(addr, Some((cfg, config.reset_key)), config.early_data)?,
        })
    }

    /// A client endpoint bound to `addr` (`0.0.0.0:0` for any port).
    pub fn client(addr: SocketAddr) -> io::Result<Self> {
        Ok(Self {
            sh: proto::bind(addr, None, false)?,
        })
    }

    /// The next connection whose handshake has completed, or, when the
    /// server takes early data ([`ServerConfig::early_data`]), whose first
    /// flight is in. `None` once the endpoint is closed.
    ///
    /// Cancel safe: a connection is handed over only when the future
    /// completes; one that arrives after it was dropped waits for the next
    /// call.
    pub async fn accept(&self) -> Option<Connection> {
        let h = self.sh.accept().await?;
        Some(Connection {
            sh: self.sh.clone(),
            h,
        })
    }

    /// Connects to `to`, checking its certificate against `server_name`.
    /// Fails with `GIONG-E3602`.
    ///
    /// Cancel safe: dropping the future closes the connection it was
    /// making. The peer may have seen the handshake start.
    pub async fn connect(
        &self,
        to: SocketAddr,
        server_name: &str,
        config: &ClientConfig,
    ) -> io::Result<Connection> {
        let h = self
            .sh
            .connect(config.to_proto()?, to, server_name, false)
            .await?;
        Ok(Connection {
            sh: self.sh.clone(),
            h,
        })
    }

    /// Connects as [`connect`](Self::connect) does, but returns at once
    /// when `config` allows early data ([`ClientConfig::early_data`]) and
    /// holds a session ticket for this server: streams opened before
    /// [`Connection::handshake`] returns carry their first bytes in 0-RTT
    /// data. Send only what is harmless to do twice in them, since early
    /// data can be replayed. Without a ticket it waits for the handshake,
    /// as `connect` does.
    ///
    /// If the server refuses the early data, those streams fail with
    /// `GIONG-E3605` and nothing of them reached the server; the
    /// connection carries on.
    ///
    /// Cancel safe: dropping the future closes the connection it was
    /// making. The peer may have seen the handshake start.
    pub async fn connect_early(
        &self,
        to: SocketAddr,
        server_name: &str,
        config: &ClientConfig,
    ) -> io::Result<Connection> {
        let h = self
            .sh
            .connect(config.to_proto()?, to, server_name, true)
            .await?;
        Ok(Connection {
            sh: self.sh.clone(),
            h,
        })
    }

    /// Connections this endpoint still holds: open, closing or draining.
    pub fn connections(&self) -> usize {
        self.sh.connections()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.sh.local_addr()
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.sh.close();
    }
}

/// An established QUIC connection. Dropping the connection and all its
/// streams closes it.
pub struct Connection {
    sh: Rc<proto::Shared>,
    h: usize,
}

impl Connection {
    /// The next bidirectional stream the peer opens.
    ///
    /// Cancel safe: a stream is taken only when the future completes.
    pub async fn accept_bi(&self) -> io::Result<(SendStream, RecvStream)> {
        let id = self.sh.stream(self.h, false, Dir::Bi).await?;
        Ok(self.pair(id))
    }

    /// Opens a bidirectional stream, waiting while the peer's stream limit
    /// is reached.
    ///
    /// Cancel safe: a stream is opened only when the future completes.
    pub async fn open_bi(&self) -> io::Result<(SendStream, RecvStream)> {
        let id = self.sh.stream(self.h, true, Dir::Bi).await?;
        Ok(self.pair(id))
    }

    /// The next unidirectional stream the peer opens.
    ///
    /// Cancel safe: a stream is taken only when the future completes.
    pub async fn accept_uni(&self) -> io::Result<RecvStream> {
        let id = self.sh.stream(self.h, false, Dir::Uni).await?;
        Ok(RecvStream {
            sh: self.sh.clone(),
            h: self.h,
            id,
            done: false,
        })
    }

    /// Opens a unidirectional stream, waiting while the peer's stream limit
    /// is reached.
    ///
    /// Cancel safe: a stream is opened only when the future completes.
    pub async fn open_uni(&self) -> io::Result<SendStream> {
        let id = self.sh.stream(self.h, true, Dir::Uni).await?;
        Ok(SendStream {
            sh: self.sh.clone(),
            h: self.h,
            id,
            finished: false,
        })
    }

    fn pair(&self, id: proto::Sid) -> (SendStream, RecvStream) {
        (
            SendStream {
                sh: self.sh.clone(),
                h: self.h,
                id,
                finished: false,
            },
            RecvStream {
                sh: self.sh.clone(),
                h: self.h,
                id,
                done: false,
            },
        )
    }

    /// Waits for the end of the handshake. On a client, `true` if the
    /// server took the early data sent before it (see
    /// [`Endpoint::connect_early`]); `false` if none was sent or the server
    /// refused it, in which case the streams opened before fail with
    /// `GIONG-E3605`. On a server, `true` if the client opened a stream in
    /// early data.
    ///
    /// Cancel safe: it only waits.
    pub async fn handshake(&self) -> io::Result<bool> {
        self.sh.handshake(self.h).await
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.sh.remote_addr(self.h)
    }

    /// What this connection has sent, received and lost so far, and what
    /// it now takes the path to be. Cheap: a copy of counters the
    /// connection keeps anyway.
    pub fn stats(&self) -> ConnectionStats {
        self.sh.stats(self.h)
    }

    /// Closes the connection with an application error code and reason.
    pub fn close(&self, code: u32, reason: &[u8]) {
        self.sh.close_conn(self.h, code, reason);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.sh.release(self.h);
    }
}

/// Counters of one connection, read with [`Connection::stats`].
///
/// Counts go up for the whole life of the connection; `rtt`,
/// `congestion_window` and `mtu` describe the path the connection is on
/// now and start again when the peer moves to another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct ConnectionStats {
    /// The round-trip time, smoothed (RFC 9002, section 5.3).
    pub rtt: std::time::Duration,
    /// Bytes the congestion controller lets be in flight.
    pub congestion_window: u64,
    /// Times the congestion controller cut the window for a loss or an
    /// ECN mark.
    pub congestion_events: u64,
    /// Packets sent on this path.
    pub sent_packets: u64,
    /// Packets declared lost on this path, and sent again where needed.
    pub lost_packets: u64,
    /// Bytes in those packets.
    pub lost_bytes: u64,
    /// UDP datagrams sent.
    pub sent_datagrams: u64,
    /// Bytes in the datagrams sent.
    pub sent_bytes: u64,
    /// UDP datagrams received.
    pub received_datagrams: u64,
    /// Bytes in the datagrams received.
    pub received_bytes: u64,
    /// The largest UDP payload the path is known to carry.
    pub mtu: u16,
}

/// The application error code the peer gave when it reset a stream or asked
/// this side to stop sending on it, if `error` is that failure
/// (`GIONG-E3604`, from [`RecvStream::read`] or [`SendStream::write_all`]);
/// `None` for any other error.
pub fn reset_code(error: &io::Error) -> Option<u64> {
    error::reset_code(error)
}

/// The sending half of a stream. Dropping it before
/// [`finish`](SendStream::finish) abandons the stream with code 0
/// (`RESET_STREAM`), so that the peer never takes a stream cut short, by a
/// cancelled write say, for a whole one; a protocol with its own code for
/// that calls [`reset`](SendStream::reset) first.
pub struct SendStream {
    sh: Rc<proto::Shared>,
    h: usize,
    id: proto::Sid,
    finished: bool,
}

impl SendStream {
    /// The stream's identifier (RFC 9000, section 2.1).
    pub fn id(&self) -> u64 {
        quinn_proto::VarInt::from(self.id.id).into_inner()
    }

    /// On a client, whether the stream was opened before the handshake
    /// ended, so that what is written on it before
    /// [`Connection::handshake`] returns goes out in 0-RTT data (see
    /// [`Endpoint::connect_early`]). On a server, whether the peer opened
    /// it in early data.
    pub fn is_early(&self) -> bool {
        self.id.early
    }

    /// Abandons the stream: what was written and not yet delivered may
    /// never arrive, and the peer is told `code` (`RESET_STREAM`).
    pub fn reset(&mut self, code: u32) {
        self.finished = true;
        self.sh.reset(self.h, self.id, code);
    }

    /// Sends all of `data`, waiting while flow control holds it back.
    ///
    /// Not cancel safe: if the future is dropped, a first part of `data`
    /// may have been sent and the rest not; how much is not known.
    pub async fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
        self.sh.write(self.h, self.id, data).await
    }

    /// Ends the stream after what was written.
    pub fn finish(&mut self) -> io::Result<()> {
        self.finished = true;
        self.sh.finish(self.h, self.id)
    }
}

impl Drop for SendStream {
    fn drop(&mut self) {
        if !self.finished {
            self.sh.reset(self.h, self.id, 0);
        }
        self.sh.forget_stream(self.h, self.id, true);
        self.sh.release(self.h);
    }
}

/// The receiving half of a stream. Dropping it before the end asks the
/// peer to stop sending.
pub struct RecvStream {
    sh: Rc<proto::Shared>,
    h: usize,
    id: proto::Sid,
    done: bool,
}

impl RecvStream {
    /// The stream's identifier (RFC 9000, section 2.1).
    pub fn id(&self) -> u64 {
        quinn_proto::VarInt::from(self.id.id).into_inner()
    }

    /// Whether the stream's first bytes may have come in 0-RTT data, which
    /// can be replayed (server side; see [`ServerConfig::early_data`]). On
    /// a client, whether they went out in it.
    pub fn is_early(&self) -> bool {
        self.id.early
    }

    /// Asks the peer to stop sending, with `code` (`STOP_SENDING`). What
    /// arrives afterwards is dropped.
    pub fn stop(&mut self, code: u32) {
        if !self.done {
            self.done = true;
            self.sh.stop(self.h, self.id, code);
        }
    }

    /// The next piece of the stream, in order; `None` at its end.
    ///
    /// Cancel safe: bytes leave the stream only when the future completes.
    pub async fn read(&mut self) -> io::Result<Option<Bytes>> {
        let r = self.sh.read(self.h, self.id).await;
        if matches!(r, Ok(None) | Err(_)) {
            self.done = true;
        }
        r
    }

    /// Everything up to the end of the stream, at most `limit` bytes.
    ///
    /// Not cancel safe: if the future is dropped, what it had read is lost.
    pub async fn read_to_end(&mut self, limit: usize) -> io::Result<Vec<u8>> {
        let mut all = Vec::new();
        while let Some(b) = self.read().await? {
            if all.len() + b.len() > limit {
                self.sh.stop(self.h, self.id, 0);
                self.done = true;
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the stream is longer than the limit of {limit} bytes"),
                ));
            }
            all.extend_from_slice(&b);
        }
        Ok(all)
    }
}

impl Drop for RecvStream {
    fn drop(&mut self) {
        if !self.done {
            self.sh.stop(self.h, self.id, 0);
        }
        self.sh.forget_stream(self.h, self.id, false);
        self.sh.release(self.h);
    }
}
