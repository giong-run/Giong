//! `Content-Encoding: gzip` and `deflate`, decoded as the body arrives.
//!
//! The compressed bytes go in with [`Inflate::push`] and come out with
//! [`Inflate::pull`], which stops at a given amount of output: a small body
//! that expands to gigabytes (a "zip bomb") is stopped at the limit, never
//! held whole. The gzip trailer's CRC-32 and length are checked, and a body
//! that ends before its compressed data does is an error, so a truncated
//! response is never taken for a whole one.

use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

/// The encodings the client asks for, in `Accept-Encoding`.
pub(crate) const ACCEPT_ENCODING: &str = "gzip, deflate";

/// A content coding the client decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoding {
    Gzip,
    Deflate,
}

impl Encoding {
    /// What a `Content-Encoding` value asks for: `Ok(None)` for none
    /// (`identity`), `Err(())` for an encoding the client does not decode
    /// (the body is then given as it came, with its header).
    pub(crate) fn parse(value: &[u8]) -> Result<Option<Encoding>, ()> {
        let value = crate::parse::trim(value);
        if value.is_empty() || value.eq_ignore_ascii_case(b"identity") {
            Ok(None)
        } else if value.eq_ignore_ascii_case(b"gzip") || value.eq_ignore_ascii_case(b"x-gzip") {
            Ok(Some(Encoding::Gzip))
        } else if value.eq_ignore_ascii_case(b"deflate") {
            Ok(Some(Encoding::Deflate))
        } else {
            Err(())
        }
    }
}

/// Why a body could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// The compressed data is damaged, or ends early.
    Damaged(String),
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    /// Reading a gzip member's header. `members` counts those finished.
    GzipHeader,
    /// Waiting for the first two bytes of a `deflate` body, which say
    /// whether it has the zlib wrapper (RFC 9110 says it must; some servers
    /// send raw deflate anyway).
    DeflateStart,
    /// Compressed data.
    Data,
    /// A gzip member's trailer: CRC-32 and length.
    GzipTrailer,
    /// The deflate stream ended; nothing may follow.
    Done,
}

/// A decoder for one body.
pub(crate) struct Inflate {
    encoding: Encoding,
    phase: Phase,
    state: Box<InflateState>,
    /// Compressed bytes received and not decoded yet.
    input: Vec<u8>,
    /// Where `input` starts being unread.
    at: usize,
    crc: Crc32,
    size: u32,
    members: usize,
    /// Where `inflate` writes, before the bytes are added to the output.
    scratch: Box<[u8]>,
}

impl Inflate {
    pub(crate) fn new(encoding: Encoding) -> Self {
        Self {
            encoding,
            phase: match encoding {
                Encoding::Gzip => Phase::GzipHeader,
                Encoding::Deflate => Phase::DeflateStart,
            },
            state: InflateState::new_boxed(DataFormat::Raw),
            input: Vec::new(),
            at: 0,
            crc: Crc32::new(),
            size: 0,
            members: 0,
            scratch: vec![0; 32 * 1024].into_boxed_slice(),
        }
    }

    /// Adds compressed bytes.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        if self.at == self.input.len() {
            self.input.clear();
            self.at = 0;
        } else if self.at > 64 * 1024 && self.at * 2 > self.input.len() {
            self.input.drain(..self.at);
            self.at = 0;
        }
        self.input.extend_from_slice(bytes);
    }

    /// Whether every byte pushed has been decoded.
    pub(crate) fn is_drained(&self) -> bool {
        self.at == self.input.len()
    }

    /// Decodes into `out` until the bytes pushed so far are used up, or
    /// `out` holds `max` bytes.
    pub(crate) fn pull(&mut self, out: &mut Vec<u8>, max: usize) -> Result<(), DecodeError> {
        loop {
            let rest = &self.input[self.at..];
            match self.phase {
                Phase::GzipHeader => {
                    if rest.is_empty() {
                        return Ok(());
                    }
                    match gzip_header(rest)? {
                        None => return Ok(()),
                        Some(n) => {
                            self.at += n;
                            self.phase = Phase::Data;
                            self.crc = Crc32::new();
                            self.size = 0;
                        }
                    }
                }
                Phase::DeflateStart => {
                    if rest.len() < 2 {
                        return Ok(());
                    }
                    let (cmf, flg) = (rest[0], rest[1]);
                    let zlib = cmf & 0x0f == 8
                        && cmf >> 4 <= 7
                        && (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0
                        && flg & 0x20 == 0;
                    let format = if zlib {
                        DataFormat::Zlib
                    } else {
                        DataFormat::Raw
                    };
                    self.state = InflateState::new_boxed(format);
                    self.phase = Phase::Data;
                }
                Phase::Data => {
                    if out.len() >= max {
                        return Ok(());
                    }
                    let room = (max - out.len()).min(self.scratch.len());
                    let result = inflate(
                        &mut self.state,
                        rest,
                        &mut self.scratch[..room],
                        MZFlush::None,
                    );
                    let written = &self.scratch[..result.bytes_written];
                    out.extend_from_slice(written);
                    self.at += result.bytes_consumed;
                    if self.encoding == Encoding::Gzip {
                        self.crc.update(written);
                        self.size = self.size.wrapping_add(written.len() as u32);
                    }
                    match result.status {
                        Ok(MZStatus::StreamEnd) => {
                            self.phase = match self.encoding {
                                Encoding::Gzip => Phase::GzipTrailer,
                                Encoding::Deflate => Phase::Done,
                            };
                        }
                        Ok(_) => {
                            if result.bytes_consumed == 0 && result.bytes_written == 0 {
                                return Ok(());
                            }
                        }
                        // No progress without more input.
                        Err(MZError::Buf) => return Ok(()),
                        Err(_) => {
                            return Err(DecodeError::Damaged(format!(
                                "the {} data is damaged",
                                self.name()
                            )))
                        }
                    }
                }
                Phase::GzipTrailer => {
                    if rest.len() < 8 {
                        return Ok(());
                    }
                    let crc = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
                    let size = u32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]);
                    if crc != self.crc.value() || size != self.size {
                        return Err(DecodeError::Damaged(
                            "the gzip data does not match its checksum".into(),
                        ));
                    }
                    self.at += 8;
                    self.members += 1;
                    // Another member may follow (RFC 1952 section 2.2).
                    self.state = InflateState::new_boxed(DataFormat::Raw);
                    self.phase = Phase::GzipHeader;
                }
                Phase::Done => {
                    if rest.is_empty() {
                        return Ok(());
                    }
                    return Err(DecodeError::Damaged(format!(
                        "bytes follow the end of the {} data",
                        self.name()
                    )));
                }
            }
        }
    }

    /// Checks that the compressed data ended where the body did.
    pub(crate) fn finish(&self) -> Result<(), DecodeError> {
        let complete = match self.phase {
            Phase::Done => true,
            Phase::GzipHeader => self.members > 0 && self.is_drained(),
            _ => false,
        };
        if complete {
            Ok(())
        } else {
            Err(DecodeError::Damaged(format!(
                "the body ends before its {} data does (cut short?)",
                self.name()
            )))
        }
    }

    fn name(&self) -> &'static str {
        match self.encoding {
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
        }
    }
}

/// The length of the gzip member header at the start of `b` (RFC 1952
/// section 2.3), or `None` if more bytes are needed.
fn gzip_header(b: &[u8]) -> Result<Option<usize>, DecodeError> {
    let damaged = |m: &str| Err(DecodeError::Damaged(format!("the gzip header {m}")));
    if b.len() >= 2 && (b[0], b[1]) != (0x1f, 0x8b) {
        return damaged("is missing: the body is not gzip");
    }
    if b.len() < 10 {
        return Ok(None);
    }
    if b[2] != 8 {
        return damaged("names a compression method other than deflate");
    }
    let flags = b[3];
    if flags & 0xe0 != 0 {
        return damaged("has reserved flags set");
    }
    let mut at = 10;
    if flags & 0x04 != 0 {
        // FEXTRA
        let Some(len) = b.get(at..at + 2) else {
            return Ok(None);
        };
        at += 2 + usize::from(u16::from_le_bytes([len[0], len[1]]));
    }
    for flag in [0x08, 0x10] {
        // FNAME, FCOMMENT: zero-terminated.
        if flags & flag != 0 {
            match b.get(at..).and_then(|r| r.iter().position(|&c| c == 0)) {
                Some(end) => at += end + 1,
                None if b.len() - at.min(b.len()) > 64 * 1024 => {
                    return damaged("has a name or comment longer than 64 KiB")
                }
                None => return Ok(None),
            }
        }
    }
    if flags & 0x02 != 0 {
        // FHCRC
        at += 2;
    }
    Ok((b.len() >= at).then_some(at))
}

/// CRC-32 as gzip uses it (IEEE 802.3, reflected).
#[derive(Clone, Copy)]
struct Crc32(u32);

const CRC_TABLE: [u32; 256] = {
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

impl Crc32 {
    fn new() -> Self {
        Crc32(!0)
    }

    fn update(&mut self, bytes: &[u8]) {
        let mut c = self.0;
        for &b in bytes {
            c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    fn value(self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use miniz_oxide::deflate::{compress_to_vec, compress_to_vec_zlib};

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
        out.extend(compress_to_vec(data, 6));
        let mut crc = Crc32::new();
        crc.update(data);
        out.extend(crc.value().to_le_bytes());
        out.extend((data.len() as u32).to_le_bytes());
        out
    }

    /// Decodes `input` pushed in pieces of `step` bytes, pulling up to
    /// `max` bytes at a time.
    fn decode(encoding: Encoding, input: &[u8], step: usize) -> Result<Vec<u8>, DecodeError> {
        let mut d = Inflate::new(encoding);
        let mut out = Vec::new();
        for piece in input.chunks(step.max(1)) {
            d.push(piece);
            d.pull(&mut out, usize::MAX)?;
        }
        d.pull(&mut out, usize::MAX)?;
        d.finish()?;
        Ok(out)
    }

    fn sample() -> Vec<u8> {
        (0..200_000u32)
            .flat_map(|i| {
                format!("line {} of the sample {}\n", i % 977, i * 7919 % 13).into_bytes()
            })
            .collect()
    }

    #[test]
    fn the_crc_matches_the_standard_check_value() {
        let mut c = Crc32::new();
        c.update(b"123456789");
        assert_eq!(c.value(), 0xcbf4_3926);
    }

    #[test]
    fn gzip_zlib_and_raw_deflate_decode_however_the_input_is_split() {
        let data = sample();
        let gz = gzip(&data);
        let zlib = compress_to_vec_zlib(&data, 6);
        let raw = compress_to_vec(&data, 1);
        // Small pieces on a smaller body: the header, the trailer and the
        // stream's start are what they test.
        let small = &data[..100_000];
        for step in [1, 2, 3, 7, 10, 11] {
            assert_eq!(decode(Encoding::Gzip, &gzip(small), step).unwrap(), small);
            let zlib = compress_to_vec_zlib(small, 6);
            assert_eq!(decode(Encoding::Deflate, &zlib, step).unwrap(), small);
            let raw = compress_to_vec(small, 1);
            assert_eq!(decode(Encoding::Deflate, &raw, step).unwrap(), small);
        }
        for step in [100, 4096, 16 * 1024, usize::MAX] {
            assert_eq!(decode(Encoding::Gzip, &gz, step).unwrap(), data, "{step}");
            assert_eq!(
                decode(Encoding::Deflate, &zlib, step).unwrap(),
                data,
                "{step}"
            );
            assert_eq!(
                decode(Encoding::Deflate, &raw, step).unwrap(),
                data,
                "{step}"
            );
        }
        // Two members, and a header with every optional field.
        let mut two = gzip(b"hello ");
        two.extend(gzip(b"world"));
        assert_eq!(decode(Encoding::Gzip, &two, 3).unwrap(), b"hello world");
        let mut full = vec![
            0x1f, 0x8b, 8, 0x1e, 0, 0, 0, 0, 0, 3, 3, 0, b'x', b'y', b'z',
        ];
        full.extend(b"name.txt\0a comment\0\xab\xcd");
        full.extend(&gzip(b"payload")[10..]);
        assert_eq!(decode(Encoding::Gzip, &full, 5).unwrap(), b"payload");
        // Empty bodies compress too.
        assert_eq!(decode(Encoding::Gzip, &gzip(b""), 4).unwrap(), b"");
    }

    #[test]
    fn damaged_or_truncated_data_is_an_error() {
        let data = sample();
        let gz = gzip(&data[..50_000]);
        for cut in [0, 1, 5, 9, 10, 100, gz.len() - 8, gz.len() - 1] {
            assert!(
                decode(Encoding::Gzip, &gz[..cut], 64).is_err(),
                "cut at {cut}"
            );
        }
        let mut bad_crc = gz.clone();
        let n = bad_crc.len();
        bad_crc[n - 6] ^= 1;
        assert!(decode(Encoding::Gzip, &bad_crc, 999).is_err());
        let mut trailing = compress_to_vec_zlib(b"abc", 6);
        trailing.push(0);
        assert!(decode(Encoding::Deflate, &trailing, 999).is_err());
        assert!(decode(Encoding::Gzip, b"plain text, not gzip", 999).is_err());
        let mut flipped = gz.clone();
        flipped[200] ^= 0xff;
        assert!(decode(Encoding::Gzip, &flipped, 999).is_err());
    }

    #[test]
    fn pull_stops_at_the_limit_so_a_bomb_is_never_held() {
        // 64 MiB of zeros compress to about 64 KiB.
        let zeros = vec![0u8; 64 << 20];
        let gz = gzip(&zeros);
        assert!(gz.len() < 100_000, "{}", gz.len());
        let mut d = Inflate::new(Encoding::Gzip);
        d.push(&gz);
        let mut out = Vec::new();
        d.pull(&mut out, 1 << 20).unwrap();
        assert_eq!(out.len(), 1 << 20);
        assert!(!d.is_drained());
        // Pulled bit by bit, it all comes out, and checks.
        let mut total = out.len();
        loop {
            out.clear();
            d.pull(&mut out, 1 << 20).unwrap();
            if out.is_empty() {
                break;
            }
            total += out.len();
        }
        assert_eq!(total, 64 << 20);
        d.finish().unwrap();
    }

    #[test]
    fn content_encoding_values_are_read() {
        assert_eq!(Encoding::parse(b" GZIP "), Ok(Some(Encoding::Gzip)));
        assert_eq!(Encoding::parse(b"x-gzip"), Ok(Some(Encoding::Gzip)));
        assert_eq!(Encoding::parse(b"deflate"), Ok(Some(Encoding::Deflate)));
        assert_eq!(Encoding::parse(b"identity"), Ok(None));
        assert_eq!(Encoding::parse(b""), Ok(None));
        assert_eq!(Encoding::parse(b"br"), Err(()));
        assert_eq!(Encoding::parse(b"gzip, gzip"), Err(()));
    }

    #[test]
    fn random_input_is_an_error_or_bytes_never_a_panic() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let base = gzip(&sample()[..3000]);
        for _ in 0..3000 {
            let mut input = base.clone();
            for _ in 0..(next() % 8) {
                let i = next() as usize % input.len();
                input[i] = next() as u8;
            }
            input.truncate(next() as usize % (input.len() + 1));
            let step = 1 + next() as usize % 300;
            for encoding in [Encoding::Gzip, Encoding::Deflate] {
                let _ = decode(encoding, &input, step);
            }
        }
    }
}
