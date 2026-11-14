//! HTTP/2 frames (RFC 9113, section 4 and 6): the 9-byte frame header, the
//! payloads, and writers for the frames we send.

/// What every client sends first on an HTTP/2 connection.
pub(crate) const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// Bytes in a frame header.
pub(crate) const HEAD_LEN: usize = 9;

/// The smallest `SETTINGS_MAX_FRAME_SIZE`, and the default.
pub(crate) const DEFAULT_FRAME_SIZE: usize = 16_384;
/// The largest `SETTINGS_MAX_FRAME_SIZE` allowed.
pub(crate) const MAX_FRAME_SIZE_LIMIT: u32 = (1 << 24) - 1;
/// The largest flow-control window.
pub(crate) const MAX_WINDOW: i64 = (1 << 31) - 1;
/// The initial window of a connection and, by default, of every stream.
pub(crate) const DEFAULT_WINDOW: u32 = 65_535;

// Frame types.
pub(crate) const DATA: u8 = 0x0;
pub(crate) const HEADERS: u8 = 0x1;
pub(crate) const PRIORITY: u8 = 0x2;
pub(crate) const RST_STREAM: u8 = 0x3;
pub(crate) const SETTINGS: u8 = 0x4;
pub(crate) const PUSH_PROMISE: u8 = 0x5;
pub(crate) const PING: u8 = 0x6;
pub(crate) const GOAWAY: u8 = 0x7;
pub(crate) const WINDOW_UPDATE: u8 = 0x8;
pub(crate) const CONTINUATION: u8 = 0x9;

// Flags.
pub(crate) const END_STREAM: u8 = 0x1;
pub(crate) const ACK: u8 = 0x1;
pub(crate) const END_HEADERS: u8 = 0x4;
pub(crate) const PADDED: u8 = 0x8;
pub(crate) const PRIORITY_FLAG: u8 = 0x20;

// Settings.
pub(crate) const HEADER_TABLE_SIZE: u16 = 0x1;
pub(crate) const ENABLE_PUSH: u16 = 0x2;
pub(crate) const MAX_CONCURRENT_STREAMS: u16 = 0x3;
pub(crate) const INITIAL_WINDOW_SIZE: u16 = 0x4;
pub(crate) const MAX_FRAME_SIZE: u16 = 0x5;
pub(crate) const MAX_HEADER_LIST_SIZE: u16 = 0x6;
/// RFC 9218: the peer will not use RFC 7540's priority signals.
pub(crate) const NO_RFC7540_PRIORITIES: u16 = 0x9;

/// HTTP/2 error codes (RFC 9113, section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Reason {
    NoError = 0x0,
    Protocol = 0x1,
    Internal = 0x2,
    FlowControl = 0x3,
    SettingsTimeout = 0x4,
    StreamClosed = 0x5,
    FrameSize = 0x6,
    RefusedStream = 0x7,
    Cancel = 0x8,
    Compression = 0x9,
    Connect = 0xa,
    EnhanceYourCalm = 0xb,
    InadequateSecurity = 0xc,
    Http11Required = 0xd,
}

impl Reason {
    #[cfg(any(test, feature = "client"))]
    pub(crate) fn from_u32(code: u32) -> Option<Reason> {
        Some(match code {
            0x0 => Reason::NoError,
            0x1 => Reason::Protocol,
            0x2 => Reason::Internal,
            0x3 => Reason::FlowControl,
            0x4 => Reason::SettingsTimeout,
            0x5 => Reason::StreamClosed,
            0x6 => Reason::FrameSize,
            0x7 => Reason::RefusedStream,
            0x8 => Reason::Cancel,
            0x9 => Reason::Compression,
            0xa => Reason::Connect,
            0xb => Reason::EnhanceYourCalm,
            0xc => Reason::InadequateSecurity,
            0xd => Reason::Http11Required,
            _ => return None,
        })
    }

    /// The name RFC 9113 gives the code.
    #[cfg(any(test, feature = "client"))]
    pub(crate) fn name(self) -> &'static str {
        match self {
            Reason::NoError => "NO_ERROR",
            Reason::Protocol => "PROTOCOL_ERROR",
            Reason::Internal => "INTERNAL_ERROR",
            Reason::FlowControl => "FLOW_CONTROL_ERROR",
            Reason::SettingsTimeout => "SETTINGS_TIMEOUT",
            Reason::StreamClosed => "STREAM_CLOSED",
            Reason::FrameSize => "FRAME_SIZE_ERROR",
            Reason::RefusedStream => "REFUSED_STREAM",
            Reason::Cancel => "CANCEL",
            Reason::Compression => "COMPRESSION_ERROR",
            Reason::Connect => "CONNECT_ERROR",
            Reason::EnhanceYourCalm => "ENHANCE_YOUR_CALM",
            Reason::InadequateSecurity => "INADEQUATE_SECURITY",
            Reason::Http11Required => "HTTP_1_1_REQUIRED",
        }
    }
}

/// A frame header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) len: usize,
    pub(crate) kind: u8,
    pub(crate) flags: u8,
    pub(crate) stream: u32,
}

impl Head {
    /// Reads the 9 bytes of a frame header. The reserved bit of the stream
    /// identifier is ignored, as RFC 9113 requires.
    pub(crate) fn parse(b: &[u8]) -> Head {
        Head {
            len: (b[0] as usize) << 16 | (b[1] as usize) << 8 | b[2] as usize,
            kind: b[3],
            flags: b[4],
            stream: u32::from_be_bytes([b[5], b[6], b[7], b[8]]) & 0x7fff_ffff,
        }
    }

    pub(crate) fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

pub(crate) fn read_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Strips the padding of a DATA, HEADERS or PUSH_PROMISE payload. `Err` if
/// the padding is as long as the payload or longer (a `PROTOCOL_ERROR`).
pub(crate) fn unpad(head: &Head, payload: &[u8]) -> Result<(), ()> {
    if !head.has(PADDED) {
        return Ok(());
    }
    match payload.first() {
        Some(&pad) if (pad as usize) < payload.len() => Ok(()),
        _ => Err(()),
    }
}

/// The part of a padded payload that is not padding. Call [`unpad`] first.
pub(crate) fn unpadded<'a>(head: &Head, payload: &'a [u8]) -> &'a [u8] {
    if head.has(PADDED) {
        let pad = payload[0] as usize;
        &payload[1..payload.len() - pad]
    } else {
        payload
    }
}

/// A priority signal from HEADERS or PRIORITY: the stream depended on.
pub(crate) fn dependency(b: &[u8]) -> u32 {
    read_u32(b) & 0x7fff_ffff
}

// ------------------------------------------------------------------ writing

pub(crate) fn write_head(out: &mut Vec<u8>, len: usize, kind: u8, flags: u8, stream: u32) {
    debug_assert!(len < 1 << 24);
    out.extend_from_slice(&[(len >> 16) as u8, (len >> 8) as u8, len as u8, kind, flags]);
    out.extend_from_slice(&(stream & 0x7fff_ffff).to_be_bytes());
}

pub(crate) fn write_settings(out: &mut Vec<u8>, settings: &[(u16, u32)]) {
    write_head(out, settings.len() * 6, SETTINGS, 0, 0);
    for &(id, value) in settings {
        out.extend_from_slice(&id.to_be_bytes());
        out.extend_from_slice(&value.to_be_bytes());
    }
}

pub(crate) fn write_settings_ack(out: &mut Vec<u8>) {
    write_head(out, 0, SETTINGS, ACK, 0);
}

pub(crate) fn write_ping(out: &mut Vec<u8>, ack: bool, data: &[u8]) {
    write_head(out, 8, PING, if ack { ACK } else { 0 }, 0);
    out.extend_from_slice(&data[..8]);
}

pub(crate) fn write_goaway(out: &mut Vec<u8>, last: u32, reason: Reason, debug: &[u8]) {
    write_head(out, 8 + debug.len(), GOAWAY, 0, 0);
    out.extend_from_slice(&(last & 0x7fff_ffff).to_be_bytes());
    out.extend_from_slice(&(reason as u32).to_be_bytes());
    out.extend_from_slice(debug);
}

pub(crate) fn write_rst(out: &mut Vec<u8>, stream: u32, reason: Reason) {
    write_head(out, 4, RST_STREAM, 0, stream);
    out.extend_from_slice(&(reason as u32).to_be_bytes());
}

pub(crate) fn write_window_update(out: &mut Vec<u8>, stream: u32, increment: u32) {
    debug_assert!(increment > 0 && increment <= MAX_WINDOW as u32);
    write_head(out, 4, WINDOW_UPDATE, 0, stream);
    out.extend_from_slice(&increment.to_be_bytes());
}

/// Writes a header block as one HEADERS frame and as many CONTINUATION
/// frames as `max_frame` makes necessary.
pub(crate) fn write_headers(
    out: &mut Vec<u8>,
    stream: u32,
    block: &[u8],
    end_stream: bool,
    max_frame: usize,
) {
    let mut chunks = block.chunks(max_frame);
    let first = chunks.next().unwrap_or(&[]);
    let more = block.len() > max_frame;
    let mut flags = if end_stream { END_STREAM } else { 0 };
    if !more {
        flags |= END_HEADERS;
    }
    write_head(out, first.len(), HEADERS, flags, stream);
    out.extend_from_slice(first);
    let mut rest = chunks.peekable();
    while let Some(chunk) = rest.next() {
        let flags = if rest.peek().is_none() {
            END_HEADERS
        } else {
            0
        };
        write_head(out, chunk.len(), CONTINUATION, flags, stream);
        out.extend_from_slice(chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_round_trip() {
        let mut out = Vec::new();
        write_head(
            &mut out,
            0x01_02_03,
            HEADERS,
            END_HEADERS | END_STREAM,
            0x7fff_fffe,
        );
        assert_eq!(out.len(), HEAD_LEN);
        let head = Head::parse(&out);
        assert_eq!(
            head,
            Head {
                len: 0x01_02_03,
                kind: HEADERS,
                flags: END_HEADERS | END_STREAM,
                stream: 0x7fff_fffe
            }
        );
        // The reserved bit is ignored.
        let mut raw = out.clone();
        raw[5] |= 0x80;
        assert_eq!(Head::parse(&raw).stream, 0x7fff_fffe);
    }

    #[test]
    fn padding_is_checked_and_stripped() {
        let head = Head {
            len: 4,
            kind: DATA,
            flags: PADDED,
            stream: 1,
        };
        assert!(unpad(&head, &[2, b'a', 0, 0]).is_ok());
        assert_eq!(unpadded(&head, &[2, b'a', 0, 0]), b"a");
        assert!(unpad(&head, &[3, b'a', 0, 0]).is_ok());
        assert_eq!(unpadded(&head, &[3, 0, 0, 0]), b"");
        assert!(unpad(&head, &[4, 0, 0, 0]).is_err());
        assert!(unpad(&head, &[]).is_err());
    }

    #[test]
    fn long_header_blocks_continue() {
        let block: Vec<u8> = (0..40u8).collect();
        let mut out = Vec::new();
        write_headers(&mut out, 3, &block, true, 16);
        let mut at = 0;
        let mut kinds = Vec::new();
        let mut joined = Vec::new();
        while at < out.len() {
            let head = Head::parse(&out[at..]);
            kinds.push((head.kind, head.flags));
            joined.extend_from_slice(&out[at + HEAD_LEN..at + HEAD_LEN + head.len]);
            at += HEAD_LEN + head.len;
        }
        assert_eq!(joined, block);
        assert_eq!(
            kinds,
            [
                (HEADERS, END_STREAM),
                (CONTINUATION, 0),
                (CONTINUATION, END_HEADERS)
            ]
        );
    }
}
