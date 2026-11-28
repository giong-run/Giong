//! HTTP/3 servers that answer some requests honestly and break the
//! protocol in between, for the randomised test and the `h3_client` fuzz
//! target: what they do comes from a [`Source`], a seed or fuzz data.
//!
//! [`ask`] holds what the client owes whatever the server does: every
//! request ends, and a response that is accepted is one the server meant,
//! whole.

use std::time::Duration;

use giong_quic::{Connection, SendStream};

use super::{Client, ErrorKind};
use crate::h2::hpack::Index;
use crate::h3::frame::{self, kind, Header, Settings};
use crate::h3::qpack::Encoder;
use crate::Version;

/// Where a server's choices come from.
pub trait Source: 'static {
    /// A number under `n`, which is not 0.
    fn below(&mut self, n: usize) -> usize;

    /// A source for another connection.
    fn fork(&mut self) -> Self;

    /// One time in `n`.
    fn one_in(&mut self, n: usize) -> bool {
        self.below(n) == 0
    }

    /// Up to `max` bytes of anything.
    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let len = self.below(max + 1);
        (0..len).map(|_| self.below(256) as u8).collect()
    }
}

/// xorshift64*: deterministic, so a failure names its seed.
pub struct Seeded(pub u64);

impl Source for Seeded {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n as u64) as usize
    }

    fn fork(&mut self) -> Self {
        let seed = (self.below(1 << 31) as u64) << 31 | self.below(1 << 31) as u64;
        Seeded(seed | 1)
    }
}

/// Fuzz data, read from its start; 0 for ever once it is used up, which
/// is the choice that ends every loop here.
pub struct Data {
    bytes: Vec<u8>,
    at: usize,
}

impl Data {
    /// A source that reads `bytes`.
    pub fn new(bytes: &[u8]) -> Self {
        Data {
            bytes: bytes.to_vec(),
            at: 0,
        }
    }

    fn byte(&mut self) -> usize {
        let byte = self.bytes.get(self.at).copied().unwrap_or(0);
        self.at += 1;
        usize::from(byte)
    }
}

impl Source for Data {
    fn below(&mut self, n: usize) -> usize {
        let mut value = self.byte();
        if n > 1 << 8 {
            value = value << 8 | self.byte();
        }
        if n > 1 << 16 {
            value = value << 8 | self.byte();
        }
        value % n
    }

    /// The same data from where this connection is: connections of one
    /// input differ once they read on.
    fn fork(&mut self) -> Self {
        let at = self.at.min(self.bytes.len());
        self.byte();
        Data {
            bytes: self.bytes[at..].to_vec(),
            at: 0,
        }
    }
}

/// The body of an honest response of `n` bytes.
pub fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + i / 253) as u8).collect()
}

/// A HEADERS frame.
pub fn headers(status: Option<u16>, fields: &[(&str, &str)]) -> Vec<u8> {
    let mut block = Vec::new();
    Encoder::begin(&mut block);
    if let Some(status) = status {
        Encoder::status(status, &mut block);
    }
    for (name, value) in fields {
        Encoder::field(name.as_bytes(), value.as_bytes(), Index::Yes, &mut block);
    }
    let mut out = Vec::new();
    Header::write(&mut out, kind::HEADERS, block.len() as u64);
    out.extend_from_slice(&block);
    out
}

/// A DATA frame.
pub fn data(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    Header::write(&mut out, kind::DATA, body.len() as u64);
    out.extend_from_slice(body);
    out
}

/// A frame of a kind no one knows, which is read past.
fn unknown(src: &mut impl Source) -> Vec<u8> {
    let payload = src.bytes(40);
    let mut out = Vec::new();
    let kind = 0x21 + 0x1f * src.below(1000) as u64;
    Header::write(&mut out, kind, payload.len() as u64);
    out.extend_from_slice(&payload);
    out
}

/// A whole response a client must accept: its body is [`pattern`] of the
/// length that `x-len` and `content-length` both give.
fn honest(src: &mut impl Source) -> Vec<u8> {
    let len = match src.below(4) {
        0 => 0,
        1 => src.below(64),
        2 => src.below(5000),
        _ => src.below(120_000),
    };
    let body = pattern(len);
    let status = [200, 201, 404, 500][src.below(4)];
    let mut out = Vec::new();
    if src.one_in(4) {
        out.extend_from_slice(&unknown(src));
    }
    for _ in 0..src.below(3) {
        out.extend_from_slice(&headers(Some(103), &[("link", "</a>; rel=preload")]));
    }
    let len_text = len.to_string();
    out.extend_from_slice(&headers(
        Some(status),
        &[
            ("content-length", &len_text),
            ("x-len", &len_text),
            ("x-status", &status.to_string()),
        ],
    ));
    let mut rest = &body[..];
    while !rest.is_empty() {
        let n = (1 + src.below(20_000)).min(rest.len());
        out.extend_from_slice(&data(&rest[..n]));
        rest = &rest[n..];
        if src.one_in(8) {
            out.extend_from_slice(&unknown(src));
        }
        if src.one_in(16) {
            out.extend_from_slice(&data(b""));
        }
    }
    if src.one_in(4) {
        out.extend_from_slice(&headers(None, &[("x-trailer", "1")]));
    }
    out
}

/// Frames that do not belong on a response stream.
fn misplaced(src: &mut impl Source) -> Vec<u8> {
    let mut out = Vec::new();
    match src.below(6) {
        0 => Settings::write_frame(&mut out, 1 << 20, 0),
        1 => frame::write_goaway(&mut out, 4 * src.below(100) as u64),
        2 => {
            Header::write(&mut out, kind::PUSH_PROMISE, 3);
            out.extend_from_slice(&[0, 0, 0]);
        }
        // A frame of HTTP/2 that HTTP/3 does not have.
        3 => Header::write(&mut out, [0x02, 0x06, 0x08, 0x09][src.below(4)], 0),
        4 => {
            // A field section of anything.
            let block = src.bytes(30);
            Header::write(&mut out, kind::HEADERS, block.len() as u64);
            out.extend_from_slice(&block);
        }
        _ => Header::write(&mut out, kind::HEADERS, 1 << 40),
    }
    out
}

/// How a stream ends once its bytes are written.
enum End {
    Finish,
    Reset(u32),
    Close(u32),
}

fn code(src: &mut impl Source) -> u32 {
    match src.below(3) {
        0 => 0x100 + src.below(0x11) as u32,
        1 => 0x10b,
        _ => src.below(1 << 24) as u32,
    }
}

fn answer(src: &mut impl Source) -> (Vec<u8>, End) {
    let mut bytes = honest(src);
    if src.below(3) != 1 {
        return (bytes, End::Finish);
    }
    match src.below(6) {
        0 => bytes.truncate(src.below(bytes.len() + 1)),
        1 => bytes.extend_from_slice(&src.bytes(30)),
        2 => bytes = src.bytes(200),
        3 => {
            let mut with = misplaced(src);
            if src.one_in(2) {
                with.extend_from_slice(&bytes);
                bytes = with;
            } else {
                bytes.extend_from_slice(&with);
            }
        }
        // Two final responses.
        4 => bytes.extend_from_slice(&honest(src)),
        _ => bytes.clear(),
    }
    let end = match src.below(4) {
        1 => End::Reset(code(src)),
        2 => End::Close(code(src)),
        _ => End::Finish,
    };
    (bytes, end)
}

/// A unidirectional stream the server opens besides its control stream.
async fn own_stream(conn: &Connection, src: &mut impl Source, held: &mut Vec<SendStream>) {
    // The client allows few of these: the server asks for more than it
    // may have, and does not wait for them.
    let opened = giong::time::timeout(Duration::from_millis(20), conn.open_uni()).await;
    let Ok(Ok(mut stream)) = opened else {
        return;
    };
    let mut out = Vec::new();
    match src.below(6) {
        // QPACK streams, which may stay silent or say little.
        0 => out.push(0x02),
        1 => out.push(0x03),
        // A push stream, which was never allowed.
        2 => out.extend_from_slice(&[0x01, 0x00]),
        // A second control stream.
        3 => {
            out.push(0x00);
            Settings::write_frame(&mut out, 1 << 20, 0);
        }
        // A stream of a kind no one knows.
        4 => {
            frame::write_varint(&mut out, 0x21 + 0x1f * src.below(1000) as u64);
            out.extend_from_slice(&src.bytes(50));
        }
        _ => out = src.bytes(50),
    }
    let _ = stream.write_all(&out).await;
    if src.one_in(3) {
        let _ = stream.finish();
    }
    held.push(stream);
}

/// Serves one connection until it ends.
///
/// Cancel safe: a dropped future drops the connection and its streams,
/// which closes the connection.
pub async fn serve(conn: Connection, mut src: impl Source) {
    let mut held = Vec::new();
    let mut control = None;
    let first = match src.below(10) {
        // No control stream at all.
        1 => None,
        // One that does not start with SETTINGS.
        2 => {
            let mut out = vec![0x00];
            out.extend_from_slice(&src.bytes(30));
            Some((out, false))
        }
        _ => {
            let mut out = vec![0x00];
            Settings::write_frame(&mut out, 1 << 20, 0);
            Some((out, true))
        }
    };
    if let Some((out, usable)) = first {
        let Ok(mut stream) = conn.open_uni().await else {
            return;
        };
        let _ = stream.write_all(&out).await;
        if usable {
            control = Some(stream);
        } else {
            held.push(stream);
        }
    }
    while let Ok((mut send, mut recv)) = conn.accept_bi().await {
        let _ = recv.read_to_end(1 << 20).await;
        if src.below(12) == 1 {
            own_stream(&conn, &mut src, &mut held).await;
        }
        if src.below(20) == 1 {
            if let Some(control) = control.as_mut() {
                let out = match src.below(4) {
                    0 => {
                        let mut out = Vec::new();
                        frame::write_goaway(&mut out, send.id() + 4);
                        out
                    }
                    1 => {
                        let mut out = Vec::new();
                        frame::write_goaway(&mut out, src.below(64) as u64);
                        out
                    }
                    2 => misplaced(&mut src),
                    _ => unknown(&mut src),
                };
                let _ = control.write_all(&out).await;
                if src.one_in(4) {
                    let _ = control.finish();
                }
            }
        }
        let (bytes, end) = answer(&mut src);
        let _ = send.write_all(&bytes).await;
        match end {
            End::Finish => {
                let _ = send.finish();
            }
            End::Reset(code) => send.reset(code),
            End::Close(code) => {
                conn.close(code, b"");
                return;
            }
        }
    }
}

/// One request to a server of [`serve`]: it ends in time, and an answer
/// that is accepted is one the server meant, whole. `true` if it was
/// answered.
///
/// The client's own timeout must be shorter than 20 s and long enough for
/// loopback: the servers here answer at once, so a request that times out
/// is a request the client lost.
///
/// Cancel safe: a dropped future gives the request up, as dropping
/// `RequestBuilder::send` does.
pub async fn ask(client: &Client, url: &str) -> bool {
    let sent = giong::time::timeout(Duration::from_secs(20), client.get(url).send()).await;
    match sent.expect("a request never ends") {
        Ok(res) => {
            assert_eq!(res.version(), Version::Http3);
            let len: usize = res
                .header("x-len")
                .expect("accepted a response no one sent")
                .parse()
                .unwrap();
            assert_eq!(res.header("x-status").unwrap(), res.status().to_string());
            assert!(res.body() == &pattern(len)[..], "a body that was not sent");
            true
        }
        Err(error) => {
            assert_ne!(error.kind(), ErrorKind::Timeout, "{error}");
            false
        }
    }
}
