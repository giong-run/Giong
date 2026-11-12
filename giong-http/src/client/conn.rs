//! One connection: opening it, sending a request on it, and reading the
//! response.

use std::io;
use std::ops::Deref;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use giong::net::TcpStream;
use giong::RecvBuf;
use giong_tls::{TlsConnector, TlsStream};

use super::error::{Error, ErrorKind};
use super::head::{parse_head, Framing, Head};
use super::url::Origin;
use crate::chunked::{Decoder, Progress};
use crate::parse::find_head_end;

/// The socket under a connection.
pub(crate) enum Stream {
    Tcp(TcpStream),
    Tls(Box<TlsStream>),
}

/// Received bytes, from either socket.
enum Chunk {
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
    /// The next bytes; empty at the end of the stream.
    async fn recv(&self) -> io::Result<Chunk> {
        match self {
            Stream::Tcp(s) => s.recv().await.map(Chunk::Pooled),
            Stream::Tls(s) => s.recv().await.map(Chunk::Owned),
        }
    }

    async fn write_all(&self, buf: Vec<u8>) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.write_all(buf).await.0,
            Stream::Tls(s) => s.write_all(buf).await.0,
        }
    }

    fn tcp(&self) -> &TcpStream {
        match self {
            Stream::Tcp(s) => s,
            Stream::Tls(s) => s.tcp(),
        }
    }

    /// Whether an end of stream just received can be trusted to be the
    /// server's: over TLS only a `close_notify` alert is authenticated, and
    /// a TCP close without one may come from anyone on the path.
    fn ended_cleanly(&self) -> bool {
        match self {
            Stream::Tcp(_) => true,
            Stream::Tls(s) => s.received_close_notify(),
        }
    }
}

/// An open connection to one origin.
pub(crate) struct Conn {
    stream: Stream,
    /// When it last finished a response.
    pub(crate) idle_since: Instant,
}

/// The limits that apply while a response is read.
pub(crate) struct Limits {
    pub(crate) max_head_size: usize,
    pub(crate) max_response_size: usize,
}

/// A response read whole.
pub(crate) struct Exchange {
    pub(crate) head: Head,
    /// The head's bytes, then the body (decoded, if it was chunked).
    pub(crate) data: Vec<u8>,
    /// The connection can carry another request.
    pub(crate) reusable: bool,
}

/// Why an exchange failed.
pub(crate) enum Failure {
    /// No byte of a response arrived: on a kept-alive connection, the server
    /// may have closed it before it read the request.
    NothingReceived(Error),
    Other(Error),
}

impl Failure {
    pub(crate) fn into_error(self) -> Error {
        match self {
            Failure::NothingReceived(e) | Failure::Other(e) => e,
        }
    }
}

impl Conn {
    /// Opens a connection to `origin`, with TLS when it is `https`.
    pub(crate) async fn open(
        origin: &Origin,
        tls: Option<&TlsConnector>,
        connect_timeout: Duration,
    ) -> Result<Conn, Error> {
        let connect = async {
            let tcp = TcpStream::connect((origin.host.as_str(), origin.port))
                .await
                .map_err(|e| {
                    Error::io(
                        ErrorKind::Connect,
                        format!("cannot connect to {origin} (is the host name right, and the server up?)"),
                        e,
                    )
                })?;
            let _ = tcp.set_nodelay(true);
            let stream = match tls {
                None => Stream::Tcp(tcp),
                Some(connector) => {
                    let tls = connector.connect(&origin.host, tcp).await.map_err(|e| {
                        Error::io(
                            ErrorKind::Connect,
                            format!("the TLS handshake with {origin} failed"),
                            e,
                        )
                    })?;
                    Stream::Tls(Box::new(tls))
                }
            };
            Ok(Conn {
                stream,
                idle_since: Instant::now(),
            })
        };
        match giong::time::timeout(connect_timeout, connect).await {
            Ok(result) => result,
            Err(_) => Err(Error::new(
                ErrorKind::Connect,
                format!(
                    "connecting to {origin} took longer than the connect timeout of {connect_timeout:?}; \
                     check that the host is reachable from here (a firewall that drops packets \
                     looks like this), or raise ClientBuilder::connect_timeout"
                ),
            )),
        }
    }

    /// Whether the server closed the connection, or it failed, while it sat
    /// idle. Data alone does not count: a TLS server may send session
    /// tickets after the handshake.
    pub(crate) fn is_closed_by_peer(&self) -> bool {
        let mut fd = libc::pollfd {
            fd: self.stream.tcp().as_raw_fd(),
            events: libc::POLLRDHUP,
            revents: 0,
        };
        // SAFETY: `fd` is one valid `pollfd` for a descriptor this connection
        // owns, and a timeout of 0 makes `poll` return at once.
        let n = unsafe { libc::poll(&mut fd, 1, 0) };
        n != 0
    }

    /// Sends `request` and reads the whole response to it.
    pub(crate) async fn exchange(
        &mut self,
        request: Vec<u8>,
        head_request: bool,
        limits: &Limits,
        what: &str,
    ) -> Result<Exchange, Failure> {
        if let Err(e) = self.stream.write_all(request).await {
            return Err(Failure::NothingReceived(Error::io(
                ErrorKind::Closed,
                format!("{what}: sending the request failed"),
                e,
            )));
        }
        let mut buf: Vec<u8> = Vec::new();
        let mut received_any = false;
        let mut scanned: usize = 0;
        let mut informational = 0;

        // The head, after any 1xx (informational) responses.
        let head = loop {
            let found = find_head_end(&buf, scanned.saturating_sub(3));
            if found.unwrap_or(buf.len()) > limits.max_head_size {
                return Err(Failure::Other(Error::new(
                    ErrorKind::TooLarge,
                    format!(
                        "{what}: the response head is longer than {} bytes (ClientBuilder::max_head_size)",
                        limits.max_head_size
                    ),
                )));
            }
            if let Some(end) = found {
                let head = parse_head(&buf[..end]).map_err(|m| protocol(what, &m))?;
                if head.status == 101 {
                    return Err(protocol(
                        what,
                        "the server switched protocols (101), which this client does not support",
                    ));
                }
                if head.status < 200 {
                    informational += 1;
                    if informational > 32 {
                        return Err(protocol(what, "the server sent more than 32 1xx responses"));
                    }
                    buf.drain(..end);
                    scanned = 0;
                    continue;
                }
                break head;
            }
            // A bare line feed would never end the head: say so now rather
            // than wait for the server to close.
            if (scanned..buf.len()).any(|at| buf[at] == b'\n' && (at == 0 || buf[at - 1] != b'\r'))
            {
                return Err(protocol(
                    what,
                    "a line of the head ends in a bare line feed",
                ));
            }
            scanned = buf.len();
            let chunk = self.recv(what, received_any, "the response head").await?;
            received_any = true;
            buf.extend_from_slice(&chunk);
        };

        let head_len = head.len;
        let mut reusable = !head.close;
        let too_large = |what: &str| {
            Failure::Other(Error::new(
                ErrorKind::TooLarge,
                format!(
                    "{what}: the response body is larger than {} bytes (ClientBuilder::max_response_size)",
                    limits.max_response_size
                ),
            ))
        };
        match head.framing(head_request) {
            Framing::None => {
                reusable &= buf.len() == head_len;
                buf.truncate(head_len);
            }
            Framing::Length(n) => {
                if n > limits.max_response_size as u64 {
                    return Err(too_large(what));
                }
                let want = head_len + n as usize;
                buf.reserve(want.saturating_sub(buf.len()));
                while buf.len() < want {
                    let chunk = self.recv(what, true, "the response body").await?;
                    buf.extend_from_slice(&chunk);
                }
                // Bytes after the body belong to no request.
                reusable &= buf.len() == want;
                buf.truncate(want);
            }
            Framing::Chunked => {
                let mut decoder = Decoder::new(limits.max_response_size);
                loop {
                    match decoder.feed(&buf[head_len..]) {
                        Ok(Progress::Done(n)) => {
                            reusable &= head_len + n == buf.len();
                            buf.truncate(head_len);
                            buf.extend_from_slice(&decoder.body);
                            break;
                        }
                        Ok(Progress::NeedMore) => {
                            let chunk = self.recv(what, true, "the chunked response body").await?;
                            buf.extend_from_slice(&chunk);
                        }
                        Err(413) => return Err(too_large(what)),
                        Err(_) => {
                            return Err(protocol(what, "the chunked body is malformed"));
                        }
                    }
                }
            }
            Framing::UntilClose => {
                reusable = false;
                if buf.len() - head_len > limits.max_response_size {
                    return Err(too_large(what));
                }
                loop {
                    let chunk = match self.stream.recv().await {
                        Ok(chunk) => chunk,
                        Err(e) => {
                            return Err(Failure::Other(Error::io(
                                ErrorKind::Closed,
                                format!("{what}: reading the response body failed"),
                                e,
                            )))
                        }
                    };
                    if chunk.is_empty() {
                        // Without close_notify the body may have been cut
                        // short on the way (a truncation attack).
                        if !self.stream.ended_cleanly() {
                            return Err(Failure::Other(Error::new(
                                ErrorKind::Closed,
                                format!(
                                    "{what}: the server closed the connection without a TLS \
                                     close_notify, so the response body, which ends where \
                                     the connection does, may have been cut short"
                                ),
                            )));
                        }
                        break;
                    }
                    if buf.len() - head_len + chunk.len() > limits.max_response_size {
                        return Err(too_large(what));
                    }
                    buf.extend_from_slice(&chunk);
                }
            }
        }
        self.idle_since = Instant::now();
        Ok(Exchange {
            head,
            data: buf,
            reusable,
        })
    }

    /// The next bytes of the response; the end of the stream is an error.
    async fn recv(&self, what: &str, received_any: bool, part: &str) -> Result<Chunk, Failure> {
        let fail = |e: Error| {
            if received_any {
                Failure::Other(e)
            } else {
                Failure::NothingReceived(e)
            }
        };
        match self.stream.recv().await {
            Ok(chunk) if !chunk.is_empty() => Ok(chunk),
            Ok(_) if !received_any => Err(fail(Error::new(
                ErrorKind::Closed,
                format!("{what}: the server closed the connection without answering"),
            ))),
            Ok(_) => Err(fail(Error::new(
                ErrorKind::Closed,
                format!("{what}: the server closed the connection in the middle of {part}"),
            ))),
            Err(e) => Err(fail(Error::io(
                ErrorKind::Closed,
                format!("{what}: reading {part} failed"),
                e,
            ))),
        }
    }
}

fn protocol(what: &str, message: &str) -> Failure {
    Failure::Other(Error::new(
        ErrorKind::Protocol,
        format!("{what}: the response is not valid HTTP/1.1: {message}"),
    ))
}
