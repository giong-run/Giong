//! Configuration, independent of the engine underneath.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use crate::error;

/// Transport settings of a connection (RFC 9000, section 18.2). The
/// defaults suit a server with many connections; raise the windows for
/// fewer, faster connections.
#[derive(Clone, Debug)]
pub struct TransportConfig {
    pub(crate) max_bidi_streams: u32,
    pub(crate) max_uni_streams: u32,
    pub(crate) stream_window: u32,
    pub(crate) connection_window: u32,
    pub(crate) idle_timeout: Duration,
    pub(crate) keep_alive: Option<Duration>,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            max_bidi_streams: 100,
            max_uni_streams: 100,
            stream_window: 1 << 20,
            connection_window: 8 << 20,
            idle_timeout: Duration::from_secs(30),
            keep_alive: None,
        }
    }
}

impl TransportConfig {
    /// Streams of each kind the peer may have open at once (default 100).
    pub fn max_streams(mut self, bidi: u32, uni: u32) -> Self {
        self.max_bidi_streams = bidi;
        self.max_uni_streams = uni;
        self
    }

    /// Bytes the peer may send ahead on one stream (default 1 MiB) and on
    /// the whole connection (default 8 MiB).
    pub fn windows(mut self, stream: u32, connection: u32) -> Self {
        self.stream_window = stream;
        self.connection_window = connection.max(stream);
        self
    }

    /// A connection with no packet for this long is closed (default 30 s).
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Sends a keep-alive this often while idle (default never).
    pub fn keep_alive(mut self, interval: Option<Duration>) -> Self {
        self.keep_alive = interval;
        self
    }

    pub(crate) fn to_proto(&self) -> io::Result<quinn_proto::TransportConfig> {
        let mut t = quinn_proto::TransportConfig::default();
        let idle = quinn_proto::IdleTimeout::try_from(self.idle_timeout).map_err(|_| {
            error::config(
                format!("idle timeout {:?} is out of range", self.idle_timeout),
                "use an idle timeout under 2^62 milliseconds, such as 30 seconds.",
            )
        })?;
        t.max_concurrent_bidi_streams(self.max_bidi_streams.into())
            .max_concurrent_uni_streams(self.max_uni_streams.into())
            .stream_receive_window(self.stream_window.into())
            .receive_window(self.connection_window.into())
            .send_window(u64::from(self.connection_window))
            .max_idle_timeout(Some(idle))
            .keep_alive_interval(self.keep_alive);
        Ok(t)
    }
}

/// What a server endpoint needs: its TLS 1.3 configuration (certificate,
/// ALPN protocols) and transport settings.
#[derive(Clone)]
pub struct ServerConfig {
    pub(crate) tls: Arc<rustls::ServerConfig>,
    pub(crate) transport: TransportConfig,
}

impl ServerConfig {
    /// Takes a rustls configuration that allows TLS 1.3 and names at least
    /// one ALPN protocol (`h3` for HTTP/3). Refused with `GIONG-E3601`
    /// otherwise.
    pub fn new(tls: rustls::ServerConfig) -> io::Result<Self> {
        if tls.alpn_protocols.is_empty() {
            return Err(error::config(
                "the server's TLS configuration names no ALPN protocol, which QUIC requires",
                "set alpn_protocols on the rustls::ServerConfig, for example \
                 vec![b\"h3\".to_vec()] for HTTP/3.",
            ));
        }
        let cfg = Self {
            tls: Arc::new(tls),
            transport: TransportConfig::default(),
        };
        cfg.to_proto()?;
        Ok(cfg)
    }

    /// Replaces the transport settings.
    pub fn transport(mut self, transport: TransportConfig) -> Self {
        self.transport = transport;
        self
    }

    pub(crate) fn to_proto(&self) -> io::Result<quinn_proto::ServerConfig> {
        let crypto = quinn_proto::crypto::rustls::QuicServerConfig::try_from(self.tls.clone())
            .map_err(|e| {
                error::config(
                    format!("the server's TLS configuration cannot carry QUIC: {e}"),
                    "enable TLS 1.3 (with_protocol_versions(&[&rustls::version::TLS13])) and a \
                     TLS 1.3 cipher suite that QUIC allows.",
                )
            })?;
        let mut cfg = quinn_proto::ServerConfig::with_crypto(Arc::new(crypto));
        cfg.transport_config(Arc::new(self.transport.to_proto()?));
        Ok(cfg)
    }
}

/// What a client needs to connect: its TLS 1.3 configuration (trusted roots,
/// ALPN protocols) and transport settings.
#[derive(Clone)]
pub struct ClientConfig {
    pub(crate) tls: Arc<rustls::ClientConfig>,
    pub(crate) transport: TransportConfig,
}

impl ClientConfig {
    /// Takes a rustls configuration that allows TLS 1.3 and names at least
    /// one ALPN protocol. Refused with `GIONG-E3601` otherwise.
    pub fn new(tls: rustls::ClientConfig) -> io::Result<Self> {
        if tls.alpn_protocols.is_empty() {
            return Err(error::config(
                "the client's TLS configuration names no ALPN protocol, which QUIC requires",
                "set alpn_protocols on the rustls::ClientConfig to the protocol the server \
                 speaks, for example vec![b\"h3\".to_vec()].",
            ));
        }
        let cfg = Self {
            tls: Arc::new(tls),
            transport: TransportConfig::default(),
        };
        cfg.to_proto()?;
        Ok(cfg)
    }

    /// Replaces the transport settings.
    pub fn transport(mut self, transport: TransportConfig) -> Self {
        self.transport = transport;
        self
    }

    pub(crate) fn to_proto(&self) -> io::Result<quinn_proto::ClientConfig> {
        let crypto = quinn_proto::crypto::rustls::QuicClientConfig::try_from(self.tls.clone())
            .map_err(|e| {
                error::config(
                    format!("the client's TLS configuration cannot carry QUIC: {e}"),
                    "enable TLS 1.3 and a TLS 1.3 cipher suite that QUIC allows.",
                )
            })?;
        let mut cfg = quinn_proto::ClientConfig::new(Arc::new(crypto));
        cfg.transport_config(Arc::new(self.transport.to_proto()?));
        Ok(cfg)
    }
}
