//! HTTP/3 framing (RFC 9114, sections 6 and 7): variable-length integers,
//! frame headers, the SETTINGS and GOAWAY payloads, unidirectional stream
//! types and error codes.
//!
//! Everything here works on bytes that are already in memory. A caller
//! reading from a QUIC stream keeps what it has not consumed and asks again
//! when more arrives: [`Need::More`] says so.

use std::fmt;

/// HTTP/3 error codes (RFC 9114, section 8.1) and QPACK's (RFC 9204,
/// section 6), as sent in `RESET_STREAM`, `STOP_SENDING` and
/// `CONNECTION_CLOSE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub(crate) enum Code {
    NoError = 0x100,
    GeneralProtocolError = 0x101,
    InternalError = 0x102,
    StreamCreationError = 0x103,
    ClosedCriticalStream = 0x104,
    FrameUnexpected = 0x105,
    FrameError = 0x106,
    ExcessiveLoad = 0x107,
    IdError = 0x108,
    SettingsError = 0x109,
    MissingSettings = 0x10a,
    RequestRejected = 0x10b,
    RequestCancelled = 0x10c,
    RequestIncomplete = 0x10d,
    MessageError = 0x10e,
    ConnectError = 0x10f,
    VersionFallback = 0x110,
    QpackDecompressionFailed = 0x200,
    QpackEncoderStreamError = 0x201,
    QpackDecoderStreamError = 0x202,
}

/// What breaks a connection or a stream: the code to close it with, and
/// what happened, for the log and the reason phrase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Error {
    pub(crate) code: Code,
    pub(crate) what: &'static str,
}

impl Error {
    pub(crate) const fn new(code: Code, what: &'static str) -> Self {
        Self { code, what }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?})", self.what, self.code)
    }
}

/// The largest value a variable-length integer holds: 2^62 - 1.
pub(crate) const VARINT_MAX: u64 = (1 << 62) - 1;

/// Reads a variable-length integer (RFC 9000, section 16) at the start of
/// `buf`: the value and the bytes it took, or `None` if `buf` ends first.
/// Any encoding is accepted, the shortest or not, as QUIC requires.
pub(crate) fn read_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let len = 1usize << (first >> 6);
    let bytes = buf.get(..len)?;
    let mut value = u64::from(first & 0x3f);
    for &b in &bytes[1..] {
        value = (value << 8) | u64::from(b);
    }
    Some((value, len))
}

/// Appends `value` as a variable-length integer in its shortest form.
///
/// # Panics
///
/// If `value` is above [`VARINT_MAX`]: no length or identifier we write is.
pub(crate) fn write_varint(out: &mut Vec<u8>, value: u64) {
    if value < 1 << 6 {
        out.push(value as u8);
    } else if value < 1 << 14 {
        out.extend_from_slice(&(value as u16 | 0x4000).to_be_bytes());
    } else if value < 1 << 30 {
        out.extend_from_slice(&(value as u32 | 0x8000_0000).to_be_bytes());
    } else {
        assert!(value <= VARINT_MAX, "varint out of range");
        out.extend_from_slice(&(value | 0xc000_0000_0000_0000).to_be_bytes());
    }
}

/// The kinds of unidirectional stream (RFC 9114, section 6.2; RFC 9204,
/// section 4.2), told by the integer a stream starts with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StreamType {
    Control,
    Push,
    QpackEncoder,
    QpackDecoder,
    /// Reserved (`0x1f * N + 0x21`) or unknown: the receiver stops reading
    /// it, without an error for the connection.
    Unknown(u64),
}

impl StreamType {
    pub(crate) const CONTROL: u64 = 0x00;
    pub(crate) const PUSH: u64 = 0x01;
    pub(crate) const QPACK_ENCODER: u64 = 0x02;
    pub(crate) const QPACK_DECODER: u64 = 0x03;

    pub(crate) fn from_id(id: u64) -> Self {
        match id {
            Self::CONTROL => Self::Control,
            Self::PUSH => Self::Push,
            Self::QPACK_ENCODER => Self::QpackEncoder,
            Self::QPACK_DECODER => Self::QpackDecoder,
            other => Self::Unknown(other),
        }
    }
}

/// Frame types (RFC 9114, section 7.2).
pub(crate) mod kind {
    pub(crate) const DATA: u64 = 0x00;
    pub(crate) const HEADERS: u64 = 0x01;
    pub(crate) const CANCEL_PUSH: u64 = 0x03;
    pub(crate) const SETTINGS: u64 = 0x04;
    pub(crate) const PUSH_PROMISE: u64 = 0x05;
    pub(crate) const GOAWAY: u64 = 0x07;
    pub(crate) const MAX_PUSH_ID: u64 = 0x0d;
}

/// `true` for the frame types of HTTP/2 that HTTP/3 has no use for
/// (PRIORITY, PING, WINDOW_UPDATE, CONTINUATION): receiving one is a
/// connection error of type `H3_FRAME_UNEXPECTED` (section 7.2.8).
pub(crate) fn is_h2_only(kind: u64) -> bool {
    matches!(kind, 0x02 | 0x06 | 0x08 | 0x09)
}

/// Where a frame may appear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum On {
    /// The peer's control stream.
    Control,
    /// A request stream, as the server reads it.
    Request,
}

/// A frame header: the type and the length of the payload that follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) kind: u64,
    pub(crate) len: u64,
    /// Bytes the header itself took.
    pub(crate) size: usize,
}

/// What reading from a buffer came to when it was not an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need<T> {
    /// Parsed.
    Got(T),
    /// The buffer ends before the item does: read more and ask again.
    More,
}

impl Header {
    /// Reads a frame header at the start of `buf`, and checks that a server
    /// may receive that frame on that kind of stream (section 7.2: the table
    /// of frames and stream types). Unknown types are fine everywhere: the
    /// caller skips their payload.
    pub(crate) fn read(buf: &[u8], on: On) -> Result<Need<Header>, Error> {
        let Some((kind, a)) = read_varint(buf) else {
            return Ok(Need::More);
        };
        let Some((len, b)) = read_varint(&buf[a..]) else {
            return Ok(Need::More);
        };
        let unexpected = |what| Err(Error::new(Code::FrameUnexpected, what));
        if is_h2_only(kind) {
            return unexpected("an HTTP/2 frame type that HTTP/3 does not have");
        }
        match (kind, on) {
            (kind::DATA | kind::HEADERS, On::Control) => {
                return unexpected("DATA or HEADERS on the control stream");
            }
            (
                kind::CANCEL_PUSH | kind::SETTINGS | kind::GOAWAY | kind::MAX_PUSH_ID,
                On::Request,
            ) => {
                return unexpected("a control frame on a request stream");
            }
            // A client never sends PUSH_PROMISE (section 7.2.5).
            (kind::PUSH_PROMISE, _) => return unexpected("PUSH_PROMISE from a client"),
            _ => {}
        }
        Ok(Need::Got(Header {
            kind,
            len,
            size: a + b,
        }))
    }

    /// Appends the header of a frame of type `kind` with `len` bytes of
    /// payload.
    pub(crate) fn write(out: &mut Vec<u8>, kind: u64, len: u64) {
        write_varint(out, kind);
        write_varint(out, len);
    }
}

/// Setting identifiers (RFC 9114, section 7.2.4.1; RFC 9204, section 5).
pub(crate) mod setting {
    pub(crate) const QPACK_MAX_TABLE_CAPACITY: u64 = 0x01;
    pub(crate) const MAX_FIELD_SECTION_SIZE: u64 = 0x06;
    pub(crate) const QPACK_BLOCKED_STREAMS: u64 = 0x07;
}

/// The settings a peer announced. What it did not send has its default.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Settings {
    /// The largest dynamic table its QPACK decoder allows our encoder: 0
    /// unless it says otherwise.
    pub(crate) qpack_max_table_capacity: u64,
    /// The largest field section it accepts: unlimited unless it says
    /// otherwise.
    pub(crate) max_field_section_size: u64,
    /// Streams that may wait for dynamic table entries: 0 by default.
    pub(crate) qpack_blocked_streams: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            qpack_max_table_capacity: 0,
            max_field_section_size: VARINT_MAX,
            qpack_blocked_streams: 0,
        }
    }
}

/// The most settings read from one SETTINGS frame. HTTP/3 defines three
/// and a handful of extensions exist; a frame with more than this many is
/// only there to keep the parser busy.
const MAX_SETTINGS: usize = 64;

impl Settings {
    /// Parses the payload of a SETTINGS frame, all of it (section 7.2.4).
    /// A setting sent twice, one of HTTP/2's that HTTP/3 reserves, or a
    /// payload that ends inside a setting is a connection error. Unknown
    /// settings are ignored.
    pub(crate) fn parse(mut payload: &[u8]) -> Result<Self, Error> {
        let mut settings = Self::default();
        // Identifiers below 64 seen so far, to refuse a repeat; the three
        // HTTP/3 has are all below 64.
        let mut seen = 0u64;
        let mut count = 0;
        while !payload.is_empty() {
            let cut = Error::new(Code::FrameError, "SETTINGS ends inside a setting");
            let (id, a) = read_varint(payload).ok_or(cut)?;
            let (value, b) = read_varint(&payload[a..]).ok_or(cut)?;
            payload = &payload[a + b..];
            count += 1;
            if count > MAX_SETTINGS {
                return Err(Error::new(Code::ExcessiveLoad, "too many settings"));
            }
            if id < 64 {
                if seen & (1 << id) != 0 {
                    return Err(Error::new(Code::SettingsError, "a setting sent twice"));
                }
                seen |= 1 << id;
            }
            match id {
                // SETTINGS_ENABLE_PUSH and the others of HTTP/2 that have
                // no meaning here (section 7.2.4.1). 0x00 is reserved too.
                0x00 | 0x02..=0x05 => {
                    return Err(Error::new(
                        Code::SettingsError,
                        "a setting of HTTP/2 that HTTP/3 reserves",
                    ));
                }
                setting::QPACK_MAX_TABLE_CAPACITY => settings.qpack_max_table_capacity = value,
                setting::MAX_FIELD_SECTION_SIZE => settings.max_field_section_size = value,
                setting::QPACK_BLOCKED_STREAMS => settings.qpack_blocked_streams = value,
                _ => {}
            }
        }
        Ok(settings)
    }

    /// Appends a whole SETTINGS frame announcing `max_field_section_size`.
    /// The QPACK settings are left at their defaults: no dynamic table, no
    /// blocked streams.
    pub(crate) fn write_frame(out: &mut Vec<u8>, max_field_section_size: u64) {
        let mut payload = Vec::with_capacity(9);
        write_varint(&mut payload, setting::MAX_FIELD_SECTION_SIZE);
        write_varint(&mut payload, max_field_section_size.min(VARINT_MAX));
        Header::write(out, kind::SETTINGS, payload.len() as u64);
        out.extend_from_slice(&payload);
    }
}

/// Parses the payload of GOAWAY, MAX_PUSH_ID or CANCEL_PUSH: one integer
/// and nothing else (sections 7.2.3, 7.2.6, 7.2.7).
pub(crate) fn parse_single_id(payload: &[u8]) -> Result<u64, Error> {
    match read_varint(payload) {
        Some((id, used)) if used == payload.len() => Ok(id),
        _ => Err(Error::new(
            Code::FrameError,
            "a frame that holds one integer has another length",
        )),
    }
}

/// Appends a GOAWAY frame: the server will not handle requests on streams
/// from `stream_id` on (section 5.2).
pub(crate) fn write_goaway(out: &mut Vec<u8>, stream_id: u64) {
    let mut payload = Vec::with_capacity(8);
    write_varint(&mut payload, stream_id);
    Header::write(out, kind::GOAWAY, payload.len() as u64);
    out.extend_from_slice(&payload);
}
