//! What gRPC puts on an HTTP/2 stream: length-prefixed messages, gzip,
//! `grpc-timeout` and the percent-encoding of `grpc-message`.

use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::error::Code as ErrorCode;
use giong_proto::Encode;

use crate::status::{Code, Status};

/// The 5 bytes in front of each message: a flag, and the length.
pub(crate) const PREFIX: usize = 5;

/// A message as it arrived: the bytes of one frame, without the prefix.
/// Never copied out of the buffer it was received in: a message that came
/// alone owns that buffer, and messages that came together share it.
#[derive(Clone, Default)]
pub(crate) struct Payload {
    buf: Buf,
    start: usize,
    end: usize,
    /// It arrived compressed.
    compressed: bool,
}

#[derive(Clone)]
enum Buf {
    Own(Vec<u8>),
    Shared(Rc<Vec<u8>>),
}

impl Default for Buf {
    fn default() -> Self {
        Self::Own(Vec::new())
    }
}

impl Payload {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match &self.buf {
            Buf::Own(buf) => &buf[self.start..self.end],
            Buf::Shared(buf) => &buf[self.start..self.end],
        }
    }

    pub(crate) fn was_compressed(&self) -> bool {
        self.compressed
    }
}

impl Payload {
    /// A frame encoded here: the prefix, then the message.
    pub(crate) fn framed(frame: Vec<u8>) -> Self {
        debug_assert!(frame.len() >= PREFIX);
        Self {
            start: PREFIX,
            end: frame.len(),
            buf: Buf::Own(frame),
            compressed: false,
        }
    }

    /// The bytes as a frame to send: plain, or compressed with gzip when
    /// asked and there is something to compress. `max` is the largest
    /// message this side sends.
    pub(crate) fn into_frame(self, gzip: bool, max: usize) -> Result<Vec<u8>, Status> {
        let len = self.as_slice().len();
        if len > max {
            return Err(too_large("the message to send", len, max));
        }
        if gzip && len > 0 {
            let packed = gzip_compress(self.as_slice());
            let mut frame = Vec::with_capacity(PREFIX + packed.len());
            frame.push(1);
            frame.extend_from_slice(&(packed.len() as u32).to_be_bytes());
            frame.extend_from_slice(&packed);
            return Ok(frame);
        }
        let (start, end) = (self.start, self.end);
        let buf = match self.buf {
            // Encoded with room for the prefix, which is already written.
            Buf::Own(buf) if start == PREFIX && end == buf.len() => return Ok(buf),
            Buf::Own(buf) => Rc::new(buf),
            Buf::Shared(buf) => buf,
        };
        let mut frame = Vec::with_capacity(PREFIX + len);
        frame.push(0);
        frame.extend_from_slice(&(len as u32).to_be_bytes());
        frame.extend_from_slice(&buf[start..end]);
        Ok(frame)
    }
}

impl From<Vec<u8>> for Payload {
    fn from(buf: Vec<u8>) -> Self {
        Self {
            start: 0,
            end: buf.len(),
            buf: Buf::Own(buf),
            compressed: false,
        }
    }
}

/// Encodes `message` as one frame, compressed with gzip if asked and the
/// message is not empty.
///
/// `max` is the largest message this side sends.
pub(crate) fn encode_frame(
    message: &impl Encode,
    gzip: bool,
    max: usize,
) -> Result<Vec<u8>, Status> {
    let mut frame = Vec::new();
    encode_frame_into(&mut frame, message, gzip, max)?;
    Ok(frame)
}

/// Appends `message` to `out` as one frame, as [`encode_frame`] makes it.
/// `out` is as it was when the message is refused.
pub(crate) fn encode_frame_into(
    out: &mut Vec<u8>,
    message: &impl Encode,
    gzip: bool,
    max: usize,
) -> Result<(), Status> {
    let len = message.encoded_len();
    if len > max {
        return Err(too_large("the message to send", len, max));
    }
    if gzip && len > 0 {
        let mut plain = Vec::with_capacity(len);
        message.encode_raw(&mut plain);
        let packed = gzip_compress(&plain);
        out.reserve(PREFIX + packed.len());
        out.push(1);
        out.extend_from_slice(&(packed.len() as u32).to_be_bytes());
        out.extend_from_slice(&packed);
        return Ok(());
    }
    let start = out.len();
    out.reserve(PREFIX + len);
    out.push(0);
    // `max` is at most `u32::MAX`, so the length fits.
    out.extend_from_slice(&(len as u32).to_be_bytes());
    message.encode_raw(out);
    debug_assert_eq!(out.len() - start, PREFIX + len);
    Ok(())
}

pub(crate) fn too_large(what: &str, len: usize, max: usize) -> Status {
    Status::raised(
        Code::ResourceExhausted,
        ErrorCode::GrpcMessageTooLarge,
        format!("{what} is {len} bytes, more than the limit of {max} bytes"),
        "raise the limit (max_message_size on the server or the channel) on the side \
         that refused it, or send the data in several messages of a streaming call.",
    )
}

pub(crate) fn protocol(what: impl Into<String>) -> Status {
    Status::raised(
        Code::Internal,
        ErrorCode::GrpcProtocol,
        what.into(),
        "the other end did not speak gRPC as specified: check that it is a gRPC server \
         or client and that no proxy between the two rewrites the stream.",
    )
}

/// A message longer than this that is still arriving is gathered piece by
/// piece and put together once complete, rather than given room up front:
/// its length is only what the sender announced.
const STEP: usize = 16 * 1024;

/// Takes the messages out of the pieces of a body as they arrive.
pub(crate) struct Deframer {
    /// The piece being read, shared with the messages taken out of it.
    buf: Rc<Vec<u8>>,
    /// Where the bytes not yet returned start in `buf`.
    at: usize,
    /// The largest message accepted, after decompression too.
    max: usize,
    /// Whether the other side said it compresses with gzip.
    gzip: bool,
    /// The piece that arrived while `buf` ended in a message: `buf` holds
    /// that message completed with the first bytes of the piece, and the
    /// piece goes on from here once it is taken.
    after: Option<(Vec<u8>, usize)>,
    /// While a long message arrives: the pieces received after `buf`, and
    /// how many bytes from `at` complete it. Five bytes announcing `max`
    /// then hold nothing but those five bytes, where room for `max` would
    /// be memory nobody charged (and a process with a limit on its address
    /// space cannot get it, and aborts); each byte is still copied once.
    parts: Vec<Vec<u8>>,
    parts_len: usize,
    gathering: usize,
}

impl Deframer {
    pub(crate) fn new(max: usize, gzip: bool) -> Self {
        Self {
            buf: Rc::default(),
            at: 0,
            max,
            gzip,
            after: None,
            parts: Vec::new(),
            parts_len: 0,
            gathering: 0,
        }
    }

    /// Adds a piece of the body.
    pub(crate) fn push(&mut self, piece: Vec<u8>) {
        if self.gathering > 0 {
            self.parts_len += piece.len();
            self.parts.push(piece);
            let rest = self.buf.len() - self.at;
            if rest + self.parts_len >= self.gathering {
                // Complete: the message, and what came after it, in one
                // buffer of exactly that size.
                let mut joined = Vec::with_capacity(rest + self.parts_len);
                joined.extend_from_slice(&self.buf[self.at..]);
                for part in self.parts.drain(..) {
                    joined.extend_from_slice(&part);
                }
                self.buf = Rc::new(joined);
                self.at = 0;
                self.parts_len = 0;
                self.gathering = 0;
            }
            return;
        }
        if self.at == self.buf.len() {
            // Every message of the last piece is out: the buffer goes to
            // the last of them still held, or is used again.
            match Rc::get_mut(&mut self.buf) {
                Some(buf) => *buf = piece,
                None => self.buf = Rc::new(piece),
            }
        } else if let Some(head) = self.completes(&piece) {
            // A message goes on in this piece and ends in it: only the
            // message is put together, and the piece is read where it was
            // received, instead of copied behind the message's start.
            let rest = &self.buf[self.at..];
            let mut joined = Vec::with_capacity(rest.len() + head);
            joined.extend_from_slice(rest);
            joined.extend_from_slice(&piece[..head]);
            self.buf = Rc::new(joined);
            self.after = Some((piece, head));
        } else {
            // A message goes on in this piece: its start moves in front.
            match Rc::get_mut(&mut self.buf) {
                Some(buf) => {
                    buf.drain(..self.at);
                    buf.extend_from_slice(&piece);
                }
                None => {
                    let rest = &self.buf[self.at..];
                    let mut buf = Vec::with_capacity(rest.len() + piece.len());
                    buf.extend_from_slice(rest);
                    buf.extend_from_slice(&piece);
                    self.buf = Rc::new(buf);
                }
            }
        }
        self.at = 0;
    }

    /// How many bytes of `piece` complete the message that `buf` ends in,
    /// when it ends in one, the piece holds the rest of it and more, and
    /// the message is shorter than the piece: then putting the message
    /// together costs less than copying the piece.
    fn completes(&self, piece: &[u8]) -> Option<usize> {
        let rest = &self.buf[self.at..];
        let mut prefix = [0; PREFIX];
        let have = rest.len().min(PREFIX);
        prefix[..have].copy_from_slice(&rest[..have]);
        prefix[have..].copy_from_slice(piece.get(..PREFIX - have)?);
        let len = u32::from_be_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]) as usize;
        let head = (PREFIX + len).checked_sub(rest.len())?;
        (head < piece.len() && PREFIX + len < piece.len()).then_some(head)
    }

    /// The next complete message, if one has arrived.
    pub(crate) fn next(&mut self) -> Result<Option<Payload>, Status> {
        if self.at == self.buf.len() {
            if let Some((piece, at)) = self.after.take() {
                match Rc::get_mut(&mut self.buf) {
                    Some(buf) => *buf = piece,
                    None => self.buf = Rc::new(piece),
                }
                self.at = at;
            }
        }
        let rest = &self.buf[self.at..];
        let Some(prefix) = rest.get(..PREFIX) else {
            return Ok(None);
        };
        let flag = prefix[0];
        let len = u32::from_be_bytes([prefix[1], prefix[2], prefix[3], prefix[4]]) as usize;
        if flag > 1 {
            return Err(protocol(format!(
                "a message starts with the flag byte {flag:#04x}, which is neither 0 \
                 (plain) nor 1 (compressed)"
            )));
        }
        if len > self.max {
            return Err(too_large("a received message", len, self.max));
        }
        if flag == 1 && !self.gzip {
            return Err(Status::raised(
                Code::Internal,
                ErrorCode::GrpcProtocol,
                "a message is marked compressed, but no grpc-encoding said how",
                "the sender must name its compression in grpc-encoding.",
            ));
        }
        let end = PREFIX + len;
        if rest.len() < end {
            let missing = end - rest.len();
            if missing > STEP {
                // Gathered by `push` until complete.
                self.gathering = end;
            } else if let Some(buf) = Rc::get_mut(&mut self.buf) {
                // Room for the rest, so that it arrives without regrowing.
                buf.reserve(missing);
            }
            return Ok(None);
        }
        let (start, end) = (self.at + PREFIX, self.at + end);
        self.at = end;
        if flag == 1 {
            let plain = gzip_decompress(&self.buf[start..end], self.max)?;
            let mut payload = Payload::from(plain);
            payload.compressed = true;
            return Ok(Some(payload));
        }
        let buf = if start == PREFIX && end == self.buf.len() {
            // The piece is this message: it owns the buffer.
            match Rc::try_unwrap(std::mem::take(&mut self.buf)) {
                Ok(buf) => {
                    self.at = 0;
                    Buf::Own(buf)
                }
                Err(shared) => {
                    self.buf = shared;
                    Buf::Shared(self.buf.clone())
                }
            }
        } else {
            Buf::Shared(self.buf.clone())
        };
        Ok(Some(Payload {
            buf,
            start,
            end,
            compressed: false,
        }))
    }

    /// The body ended: nothing may be left over.
    pub(crate) fn finish(&self) -> Result<(), Status> {
        let left = self.buf.len() - self.at
            + self.parts_len
            + self
                .after
                .as_ref()
                .map_or(0, |(piece, at)| piece.len() - at);
        if left == 0 {
            Ok(())
        } else {
            Err(protocol(format!(
                "the stream ended in the middle of a message ({left} bytes of an incomplete \
                 frame)"
            )))
        }
    }
}

// ----------------------------------------------------------------- gzip

fn crc32(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            table[i] = c;
            i += 1;
        }
        table
    };
    let mut crc = !0u32;
    for &b in data {
        crc = TABLE[((crc ^ u32::from(b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// One gzip member (RFC 1952) holding `data`.
pub(crate) fn gzip_compress(data: &[u8]) -> Vec<u8> {
    // Level 1: messages are small and sent once; speed matters more than
    // the last few percent.
    let deflated = miniz_oxide::deflate::compress_to_vec(data, 1);
    let mut out = Vec::with_capacity(deflated.len() + 18);
    out.extend_from_slice(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255]);
    out.extend_from_slice(&deflated);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Reads one gzip member of at most `max` bytes once inflated, checking
/// its CRC-32 and length.
pub(crate) fn gzip_decompress(data: &[u8], max: usize) -> Result<Vec<u8>, Status> {
    let bad = |what: &str| protocol(format!("a compressed message is not valid gzip: {what}"));
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        return Err(bad("no gzip header"));
    }
    let flags = data[3];
    if flags & 0xe0 != 0 {
        return Err(bad("reserved header flags are set"));
    }
    let mut at = 10;
    if flags & 4 != 0 {
        // FEXTRA: a length, then that many bytes.
        let len = data
            .get(at..at + 2)
            .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
            .ok_or_else(|| bad("the extra field is cut short"))?;
        at += 2 + len;
    }
    for flag in [8, 16] {
        // FNAME, FCOMMENT: text ended by a zero byte.
        if flags & flag != 0 {
            let end = data
                .get(at..)
                .and_then(|rest| rest.iter().position(|&b| b == 0))
                .ok_or_else(|| bad("a header string is cut short"))?;
            at += end + 1;
        }
    }
    if flags & 2 != 0 {
        at += 2; // FHCRC
    }
    if at + 8 > data.len() {
        return Err(bad("the header is cut short"));
    }
    let (body, tail) = data[at..].split_at(data.len() - at - 8);
    let plain = miniz_oxide::inflate::decompress_to_vec_with_limit(body, max).map_err(|e| {
        if matches!(e.status, miniz_oxide::inflate::TINFLStatus::HasMoreOutput) {
            too_large("a received message, once decompressed,", max + 1, max)
        } else {
            bad("the deflate data is damaged")
        }
    })?;
    let crc = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
    let size = u32::from_le_bytes([tail[4], tail[5], tail[6], tail[7]]);
    if crc != crc32(&plain) || size != plain.len() as u32 {
        return Err(bad("the checksum or the length does not match"));
    }
    Ok(plain)
}

// -------------------------------------------------------------- timeout

/// Parses a `grpc-timeout` value: up to 8 digits and a unit.
pub(crate) fn parse_timeout(value: &[u8]) -> Option<Duration> {
    let (unit, digits) = value.split_last()?;
    if digits.is_empty() || digits.len() > 8 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let n = digits
        .iter()
        .fold(0u64, |n, &d| n * 10 + u64::from(d - b'0'));
    Some(match unit {
        b'H' => Duration::from_secs(n * 3600),
        b'M' => Duration::from_secs(n * 60),
        b'S' => Duration::from_secs(n),
        b'm' => Duration::from_millis(n),
        b'u' => Duration::from_micros(n),
        b'n' => Duration::from_nanos(n),
        _ => return None,
    })
}

/// Whether a `content-type` names gRPC: `application/grpc`, with a codec
/// (`+proto`) or parameters, and not gRPC-Web (`application/grpc-web`),
/// whose framing differs.
pub(crate) fn is_grpc_content_type(content_type: &[u8]) -> bool {
    content_type == b"application/grpc"
        || content_type.starts_with(b"application/grpc+")
        || content_type.starts_with(b"application/grpc;")
}

/// The instant `timeout` from now. A timeout past what the clock can hold
/// (`Duration::MAX` and the like) means no deadline: a century from now.
pub(crate) fn deadline_after(timeout: Duration) -> Instant {
    const CENTURY: Duration = Duration::from_secs(100 * 365 * 24 * 3600);
    let now = Instant::now();
    now.checked_add(timeout.min(CENTURY))
        .or_else(|| now.checked_add(Duration::from_secs(365 * 24 * 3600)))
        .unwrap_or(now)
}

/// Writes a duration as a `grpc-timeout` value, rounded up to the first
/// unit in which it fits in 8 digits, so the server never gets less time
/// than the caller allowed.
pub(crate) fn format_timeout(timeout: Duration) -> String {
    const MAX: u128 = 99_999_999;
    let nanos = timeout.as_nanos();
    for (unit, per) in [
        ('n', 1u128),
        ('u', 1_000),
        ('m', 1_000_000),
        ('S', 1_000_000_000),
        ('M', 60_000_000_000),
        ('H', 3_600_000_000_000),
    ] {
        let n = nanos.div_ceil(per);
        if n <= MAX {
            return format!("{n}{unit}");
        }
    }
    format!("{MAX}H")
}

// ---------------------------------------------------------- grpc-message

/// Percent-encodes a status message for `grpc-message`: every byte outside
/// printable ASCII, and `%` itself.
pub(crate) fn encode_message(message: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    if message
        .bytes()
        .all(|b| (0x20..=0x7e).contains(&b) && b != b'%')
    {
        return message.to_owned();
    }
    let mut out = Vec::with_capacity(message.len() + 16);
    for b in message.bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'%' {
            out.push(b);
        } else {
            out.extend_from_slice(&[b'%', HEX[usize::from(b >> 4)], HEX[usize::from(b & 15)]]);
        }
    }
    // Only ASCII was written.
    String::from_utf8(out).unwrap_or_default()
}

/// Decodes a `grpc-message`. A `%` that does not start an escape is kept
/// as it is, and bytes that are not UTF-8 once decoded are replaced, as
/// the specification asks: a message is never refused.
pub(crate) fn decode_message(value: &[u8]) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(value.len());
    let mut at = 0;
    while at < value.len() {
        if value[at] == b'%' {
            if let (Some(hi), Some(lo)) = (
                value.get(at + 1).copied().and_then(hex),
                value.get(at + 2).copied().and_then(hex),
            ) {
                out.push(hi << 4 | lo);
                at += 3;
                continue;
            }
        }
        out.push(value[at]);
        at += 1;
    }
    match String::from_utf8(out) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Five bytes announcing the largest message reserve no more than a
    /// step: room grows with what arrives, not with what is announced.
    #[test]
    fn an_announced_length_reserves_only_what_has_arrived() {
        let max = 4 << 20;
        let mut d = Deframer::new(max, false);
        let mut prefix = vec![0];
        prefix.extend_from_slice(&(max as u32).to_be_bytes());
        d.push(prefix);
        assert!(d.next().unwrap().is_none());
        assert!(d.buf.capacity() <= PREFIX + STEP, "{}", d.buf.capacity());
        // The message still arrives whole, in pieces.
        let mut sent = 0;
        while sent < max {
            let n = (max - sent).min(100_000);
            d.push(vec![7; n]);
            sent += n;
            if sent < max {
                // Nothing held but the pieces received.
                assert!(d.buf.capacity() <= PREFIX + STEP);
                assert!(d.next().unwrap().is_none());
            } else {
                // Put together once, in a buffer of exactly its size.
                assert_eq!(d.buf.capacity(), PREFIX + max);
            }
        }
        let message = d.next().unwrap().expect("the whole message");
        assert_eq!(message.as_slice().len(), max);
        d.finish().unwrap();
    }

    #[test]
    fn grpc_content_types_are_told_from_grpc_web() {
        for yes in [
            "application/grpc",
            "application/grpc+proto",
            "application/grpc;x=y",
        ] {
            assert!(is_grpc_content_type(yes.as_bytes()), "{yes}");
        }
        for no in [
            "application/grpc-web",
            "application/grpc-web+proto",
            "application/grpc-web-text",
            "application/grpcx",
            "text/plain",
            "",
        ] {
            assert!(!is_grpc_content_type(no.as_bytes()), "{no}");
        }
    }

    #[test]
    fn deadlines_past_the_clock_are_a_century_away() {
        let far = deadline_after(Duration::MAX);
        assert!(far > Instant::now() + Duration::from_secs(50 * 365 * 24 * 3600));
        let near = deadline_after(Duration::from_secs(1));
        assert!(near <= Instant::now() + Duration::from_secs(1));
    }

    struct Raw(Vec<u8>);

    impl Encode for Raw {
        fn encoded_len(&self) -> usize {
            self.0.len()
        }
        fn encode_raw(&self, buf: &mut Vec<u8>) {
            buf.extend_from_slice(&self.0);
        }
    }

    #[test]
    fn frames_survive_any_split() {
        let messages: Vec<Vec<u8>> = vec![vec![], b"a".to_vec(), vec![7; 300], vec![], vec![9; 5]];
        let mut wire = Vec::new();
        for (i, m) in messages.iter().enumerate() {
            wire.extend(encode_frame(&Raw(m.clone()), i % 2 == 1, 1 << 20).unwrap());
        }
        for step in [1, 2, 3, 5, 7, 64, wire.len()] {
            let mut deframer = Deframer::new(1 << 20, true);
            let mut got = Vec::new();
            for piece in wire.chunks(step) {
                deframer.push(piece.to_vec());
                while let Some(m) = deframer.next().unwrap() {
                    got.push(m.as_slice().to_vec());
                }
            }
            deframer.finish().unwrap();
            assert_eq!(got, messages, "step {step}");
        }
    }

    #[test]
    fn messages_split_between_large_pieces_survive_any_split() {
        // Short messages in pieces larger than they are: a message cut by
        // the end of a piece is put together alone, and the next piece is
        // read where it is.
        let messages: Vec<Vec<u8>> = (0..200).map(|i| vec![i as u8; i % 37 * 3]).collect();
        let mut wire = Vec::new();
        for m in &messages {
            wire.extend(encode_frame(&Raw(m.clone()), false, 1 << 20).unwrap());
        }
        for step in [6, 50, 113, 500, 1021, 4096] {
            let mut deframer = Deframer::new(1 << 20, false);
            let mut got = Vec::new();
            let mut held = Vec::new();
            for piece in wire.chunks(step) {
                deframer.push(piece.to_vec());
                while let Some(m) = deframer.next().unwrap() {
                    got.push(m.as_slice().to_vec());
                    // Some messages are still held when the next piece
                    // arrives.
                    if got.len() % 3 == 0 {
                        held.push((got.len() - 1, m));
                    }
                }
            }
            deframer.finish().unwrap();
            assert_eq!(got, messages, "step {step}");
            for (i, m) in &held {
                assert_eq!(m.as_slice(), &messages[*i][..], "step {step}, message {i}");
            }
        }
    }

    #[test]
    fn a_body_that_ends_inside_a_message_is_refused() {
        let mut wire = encode_frame(&Raw(vec![1; 10]), false, 1000).unwrap();
        wire.extend(encode_frame(&Raw(vec![2; 300]), false, 1000).unwrap());
        let mut deframer = Deframer::new(1000, false);
        deframer.push(wire[..12].to_vec());
        assert!(deframer.next().unwrap().is_none());
        // The second piece completes the first message and cuts the second.
        deframer.push(wire[12..200].to_vec());
        assert_eq!(deframer.next().unwrap().unwrap().as_slice(), &[1; 10]);
        assert!(deframer.next().unwrap().is_none());
        assert!(deframer.finish().is_err());
    }

    #[test]
    fn a_whole_frame_in_one_piece_is_not_copied() {
        let frame = encode_frame(&Raw(vec![3; 100]), false, 1000).unwrap();
        let at = frame.as_ptr();
        let mut deframer = Deframer::new(1000, false);
        deframer.push(frame);
        let payload = deframer.next().unwrap().unwrap();
        assert_eq!(payload.as_slice().as_ptr(), at.wrapping_add(PREFIX));
    }

    #[test]
    fn bad_frames_are_refused_with_their_codes() {
        let status = |bytes: &[u8], max, gzip| {
            let mut d = Deframer::new(max, gzip);
            d.push(bytes.to_vec());
            d.next().map(|m| m.map(|m| m.as_slice().to_vec()))
        };
        let e = status(&[2, 0, 0, 0, 0], 10, false).unwrap_err();
        assert_eq!(e.error_code(), Some(ErrorCode::GrpcProtocol));
        let e = status(&[0, 0, 0, 0, 11], 10, false).unwrap_err();
        assert_eq!(e.code(), Code::ResourceExhausted);
        assert_eq!(e.error_code(), Some(ErrorCode::GrpcMessageTooLarge));
        let e = status(&[1, 0, 0, 0, 1, 0], 10, false).unwrap_err();
        assert_eq!(e.code(), Code::Internal);
        let e = status(&[1, 0, 0, 0, 1, 0], 10, true).unwrap_err();
        assert_eq!(e.error_code(), Some(ErrorCode::GrpcProtocol));
        assert!(encode_frame(&Raw(vec![0; 11]), false, 10).is_err());
        let mut d = Deframer::new(10, false);
        d.push(vec![0, 0, 0, 0, 3, 1]);
        assert!(d.next().unwrap().is_none());
        assert!(d.finish().is_err());
    }

    #[test]
    fn gzip_round_trips_and_damage_is_noticed() {
        for data in [&b""[..], b"a", &[5u8; 10_000], b"hello hello hello"] {
            let packed = gzip_compress(data);
            assert_eq!(gzip_decompress(&packed, 1 << 20).unwrap(), data);
            if !data.is_empty() {
                let mut damaged = packed.clone();
                let last = damaged.len() - 1;
                damaged[last] ^= 1;
                assert!(gzip_decompress(&damaged, 1 << 20).is_err());
                assert!(gzip_decompress(&packed[..packed.len() - 1], 1 << 20).is_err());
            }
        }
        // A bomb stops at the limit.
        let bomb = gzip_compress(&vec![0u8; 1 << 20]);
        assert!(bomb.len() < 10_000);
        let e = gzip_decompress(&bomb, 1000).unwrap_err();
        assert_eq!(e.code(), Code::ResourceExhausted);
        // Optional header fields are skipped.
        let plain = b"with a name";
        let deflated = miniz_oxide::deflate::compress_to_vec(plain, 6);
        let mut member = vec![
            0x1f,
            0x8b,
            8,
            4 | 8 | 16 | 2,
            0,
            0,
            0,
            0,
            0,
            3,
            2,
            0,
            b'x',
            b'y',
        ];
        member.extend_from_slice(b"name\0comment\0\x12\x34");
        member.extend_from_slice(&deflated);
        member.extend_from_slice(&crc32(plain).to_le_bytes());
        member.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        assert_eq!(gzip_decompress(&member, 100).unwrap(), plain);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn timeouts_parse_and_round_up() {
        assert_eq!(parse_timeout(b"1S"), Some(Duration::from_secs(1)));
        assert_eq!(
            parse_timeout(b"99999999H"),
            Some(Duration::from_secs(99_999_999 * 3600))
        );
        assert_eq!(parse_timeout(b"250m"), Some(Duration::from_millis(250)));
        assert_eq!(parse_timeout(b"7u"), Some(Duration::from_micros(7)));
        assert_eq!(parse_timeout(b"0n"), Some(Duration::ZERO));
        for bad in [
            &b""[..],
            b"S",
            b"1",
            b"1s",
            b"123456789S",
            b"-1S",
            b"1.5S",
            b" 1S",
        ] {
            assert_eq!(parse_timeout(bad), None, "{bad:?}");
        }
        assert_eq!(format_timeout(Duration::from_millis(250)), "250000u");
        assert_eq!(format_timeout(Duration::from_nanos(1)), "1n");
        assert_eq!(format_timeout(Duration::from_secs(3600)), "3600000m");
        assert_eq!(
            format_timeout(Duration::from_nanos(100_000_000_001)),
            "100001m"
        );
        for d in [
            Duration::ZERO,
            Duration::from_nanos(123_456_789_123),
            Duration::from_secs(1 << 40),
        ] {
            let back = parse_timeout(format_timeout(d).as_bytes()).unwrap();
            assert!(
                back >= d.min(Duration::from_secs(99_999_999 * 3600)),
                "{d:?}"
            );
        }
    }

    #[test]
    fn messages_are_percent_encoded() {
        assert_eq!(encode_message("plain text"), "plain text");
        assert_eq!(encode_message("100% \u{1f600}\n"), "100%25 %F0%9F%98%80%0A");
        for text in [
            "",
            "a b",
            "100%",
            "tab\there",
            "kh\u{f4}ng t\u{ec}m th\u{1ea5}y",
            "%41",
        ] {
            assert_eq!(decode_message(encode_message(text).as_bytes()), text);
        }
        assert_eq!(decode_message(b"50%"), "50%");
        assert_eq!(decode_message(b"%zz%4"), "%zz%4");
        assert_eq!(decode_message(b"%FF"), "\u{fffd}");
    }
}
