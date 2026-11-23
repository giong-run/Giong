//! QPACK, the field compression of HTTP/3 (RFC 9204), without a dynamic
//! table.
//!
//! giong announces `SETTINGS_QPACK_MAX_TABLE_CAPACITY` = 0 and never asks
//! the peer for a table either, which RFC 9204 allows in full: every field
//! line is then a reference into the static table or a literal, no stream
//! ever waits for another, and the encoder and decoder streams carry
//! nothing. [`decode`] reads the field sections a peer sends, [`Encoder`]
//! writes ours, and [`encoder_stream`] / [`decoder_stream`] check that the
//! peer's two QPACK streams say only what such a peer may be told.
//!
//! A dynamic table comes later, and only with a benchmark that shows what
//! it buys (plan for 1.3).

use std::cell::RefCell;
use std::sync::OnceLock;

use super::frame::{Code, Error};
use crate::h2::hpack::huffman;
use crate::h2::hpack::Index;

/// The static table (RFC 9204, Appendix A): index 0 is the first entry.
#[rustfmt::skip]
pub(crate) const STATIC: [(&str, &str); 99] = [
    (":authority", ""), (":path", "/"), ("age", "0"), ("content-disposition", ""),
    ("content-length", "0"), ("cookie", ""), ("date", ""), ("etag", ""),
    ("if-modified-since", ""), ("if-none-match", ""), ("last-modified", ""), ("link", ""),
    ("location", ""), ("referer", ""), ("set-cookie", ""),
    (":method", "CONNECT"), (":method", "DELETE"), (":method", "GET"), (":method", "HEAD"),
    (":method", "OPTIONS"), (":method", "POST"), (":method", "PUT"),
    (":scheme", "http"), (":scheme", "https"),
    (":status", "103"), (":status", "200"), (":status", "304"), (":status", "404"),
    (":status", "503"),
    ("accept", "*/*"), ("accept", "application/dns-message"),
    ("accept-encoding", "gzip, deflate, br"), ("accept-ranges", "bytes"),
    ("access-control-allow-headers", "cache-control"),
    ("access-control-allow-headers", "content-type"),
    ("access-control-allow-origin", "*"),
    ("cache-control", "max-age=0"), ("cache-control", "max-age=2592000"),
    ("cache-control", "max-age=604800"), ("cache-control", "no-cache"),
    ("cache-control", "no-store"), ("cache-control", "public, max-age=31536000"),
    ("content-encoding", "br"), ("content-encoding", "gzip"),
    ("content-type", "application/dns-message"), ("content-type", "application/javascript"),
    ("content-type", "application/json"),
    ("content-type", "application/x-www-form-urlencoded"),
    ("content-type", "image/gif"), ("content-type", "image/jpeg"),
    ("content-type", "image/png"), ("content-type", "text/css"),
    ("content-type", "text/html; charset=utf-8"), ("content-type", "text/plain"),
    ("content-type", "text/plain;charset=utf-8"),
    ("range", "bytes=0-"),
    ("strict-transport-security", "max-age=31536000"),
    ("strict-transport-security", "max-age=31536000; includesubdomains"),
    ("strict-transport-security", "max-age=31536000; includesubdomains; preload"),
    ("vary", "accept-encoding"), ("vary", "origin"),
    ("x-content-type-options", "nosniff"), ("x-xss-protection", "1; mode=block"),
    (":status", "100"), (":status", "204"), (":status", "206"), (":status", "302"),
    (":status", "400"), (":status", "403"), (":status", "421"), (":status", "425"),
    (":status", "500"),
    ("accept-language", ""),
    ("access-control-allow-credentials", "FALSE"),
    ("access-control-allow-credentials", "TRUE"),
    ("access-control-allow-headers", "*"),
    ("access-control-allow-methods", "get"),
    ("access-control-allow-methods", "get, post, options"),
    ("access-control-allow-methods", "options"),
    ("access-control-expose-headers", "content-length"),
    ("access-control-request-headers", "content-type"),
    ("access-control-request-method", "get"), ("access-control-request-method", "post"),
    ("alt-svc", "clear"), ("authorization", ""),
    ("content-security-policy", "script-src 'none'; object-src 'none'; base-uri 'none'"),
    ("early-data", "1"), ("expect-ct", ""), ("forwarded", ""), ("if-range", ""),
    ("origin", ""), ("purpose", "prefetch"), ("server", ""),
    ("timing-allow-origin", "*"), ("upgrade-insecure-requests", "1"), ("user-agent", ""),
    ("x-forwarded-for", ""), ("x-frame-options", "deny"), ("x-frame-options", "sameorigin"),
];

const fn failed(what: &'static str) -> Error {
    Error::new(Code::QpackDecompressionFailed, what)
}

/// Reads an integer with an `n`-bit prefix (RFC 9204, section 4.1.1) at
/// `*pos`: `Ok(None)` if `buf` ends before the integer does. Values above
/// 2^32 - 1 are refused: no length or index of ours comes near.
fn read_int(buf: &[u8], pos: &mut usize, n: u8, err: Error) -> Result<Option<u64>, Error> {
    let mask = ((1u16 << n) - 1) as u8;
    let Some(&first) = buf.get(*pos) else {
        return Ok(None);
    };
    *pos += 1;
    let first = first & mask;
    if first < mask {
        return Ok(Some(u64::from(first)));
    }
    let mut value = u64::from(mask);
    let mut shift = 0;
    loop {
        let Some(&b) = buf.get(*pos) else {
            return Ok(None);
        };
        *pos += 1;
        value += u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 28 {
            return Err(err);
        }
    }
    if value > u64::from(u32::MAX) {
        return Err(err);
    }
    Ok(Some(value))
}

/// Writes `value` with an `n`-bit prefix; `first` holds the bits above it.
fn write_int(out: &mut Vec<u8>, first: u8, n: u8, value: usize) {
    let mask = (1u16 << n) as usize - 1;
    if value < mask {
        out.push(first | value as u8);
        return;
    }
    out.push(first | mask as u8);
    let mut rest = value - mask;
    while rest >= 0x80 {
        out.push((rest as u8 & 0x7f) | 0x80);
        rest >>= 7;
    }
    out.push(rest as u8);
}

/// Scratch space for Huffman-decoded strings, shared by the connections of
/// a worker so that an idle connection keeps none.
struct Scratch {
    name: Vec<u8>,
    value: Vec<u8>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = const {
        RefCell::new(Scratch { name: Vec::new(), value: Vec::new() })
    };
}

/// Largest scratch buffer kept between field sections.
const SCRATCH_KEEP: usize = 16 * 1024;

/// Reads a string literal whose length has an `n`-bit prefix and whose
/// Huffman flag is the bit above it: the bytes in `block`, or `None` after
/// decoding them into `scratch`.
fn read_string<'a>(
    block: &'a [u8],
    pos: &mut usize,
    n: u8,
    scratch: &mut Vec<u8>,
) -> Result<Option<&'a [u8]>, Error> {
    let ends = failed("field section ends in a string");
    let huffman = *block.get(*pos).ok_or(ends)? & (1 << n) != 0;
    let len = read_int(block, pos, n, failed("string length too large"))?.ok_or(ends)?;
    let end = pos
        .checked_add(len as usize)
        .filter(|&end| end <= block.len())
        .ok_or(failed("string longer than the field section"))?;
    let raw = &block[*pos..end];
    *pos = end;
    if !huffman {
        return Ok(Some(raw));
    }
    scratch.clear();
    huffman::decode(raw, scratch).map_err(|()| failed("invalid Huffman code in a string"))?;
    Ok(None)
}

fn static_entry(index: u64) -> Result<(&'static [u8], &'static [u8]), Error> {
    let (name, value) = STATIC
        .get(index as usize)
        .ok_or(failed("index past the end of the static table"))?;
    Ok((name.as_bytes(), value.as_bytes()))
}

/// Decodes one complete encoded field section (the payload of a HEADERS
/// frame), calling `field` for each field line in order.
///
/// We allow the peer no dynamic table, so a section that refers to one
/// (a Required Insert Count above zero, a dynamic or post-base index) is a
/// connection error of type `QPACK_DECOMPRESSION_FAILED`, as is anything
/// else RFC 9204 does not allow.
pub(crate) fn decode(block: &[u8], mut field: impl FnMut(&[u8], &[u8])) -> Result<(), Error> {
    SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        let scratch = &mut *scratch;
        let result = decode_with(block, scratch, &mut field);
        for buf in [&mut scratch.name, &mut scratch.value] {
            if buf.capacity() > SCRATCH_KEEP {
                *buf = Vec::new();
            }
        }
        result
    })
}

fn decode_with(
    block: &[u8],
    scratch: &mut Scratch,
    field: &mut impl FnMut(&[u8], &[u8]),
) -> Result<(), Error> {
    let no_table = failed("reference to a dynamic table that was never allowed");
    let ends = failed("field section ends inside its prefix");
    let large = failed("integer too large in a field section");
    let mut pos = 0;
    // Section 4.5.1: Required Insert Count, then sign and Delta Base.
    let required = read_int(block, &mut pos, 8, large)?.ok_or(ends)?;
    if required != 0 {
        return Err(no_table);
    }
    let negative = *block.get(pos).ok_or(ends)? & 0x80 != 0;
    let delta = read_int(block, &mut pos, 7, large)?.ok_or(ends)?;
    // Base = Required Insert Count + Delta Base, or with the sign bit
    // Required Insert Count - Delta Base - 1, which is below zero here.
    if negative {
        return Err(failed("Base below zero"));
    }
    let _ = delta;
    let ends = failed("field section ends inside a field line");
    while pos < block.len() {
        let b = block[pos];
        if b & 0x80 != 0 {
            // Indexed field line (4.5.2): 1 T index(6).
            if b & 0x40 == 0 {
                return Err(no_table);
            }
            let index = read_int(block, &mut pos, 6, large)?.ok_or(ends)?;
            let (name, value) = static_entry(index)?;
            field(name, value);
        } else if b & 0x40 != 0 {
            // Literal field line with name reference (4.5.4): 01 N T
            // index(4), then the value.
            if b & 0x10 == 0 {
                return Err(no_table);
            }
            let index = read_int(block, &mut pos, 4, large)?.ok_or(ends)?;
            let (name, _) = static_entry(index)?;
            let value = read_string(block, &mut pos, 7, &mut scratch.value)?;
            field(name, value.unwrap_or(&scratch.value));
        } else if b & 0x20 != 0 {
            // Literal field line with literal name (4.5.6): 001 N H
            // length(3), the name, then the value.
            let name = read_string(block, &mut pos, 3, &mut scratch.name)?;
            let value = read_string(block, &mut pos, 7, &mut scratch.value)?;
            field(
                name.unwrap_or(&scratch.name),
                value.unwrap_or(&scratch.value),
            );
        } else {
            // Indexed field line with post-base index (4.5.3: 0001) or
            // literal with post-base name reference (4.5.5: 0000): both
            // point into the dynamic table.
            return Err(no_table);
        }
    }
    Ok(())
}

/// The static table's indices ordered by name, then by index: for finding
/// a name with a binary search.
fn by_name() -> &'static [u8; 99] {
    static ORDER: OnceLock<[u8; 99]> = OnceLock::new();
    ORDER.get_or_init(|| {
        let mut order = [0u8; 99];
        for (i, slot) in order.iter_mut().enumerate() {
            *slot = i as u8;
        }
        order.sort_by_key(|&i| (STATIC[i as usize].0, i));
        order
    })
}

/// The static entries named `name`: their indices, lowest first.
fn static_named(name: &[u8]) -> &'static [u8] {
    let order = by_name();
    let start = order.partition_point(|&i| STATIC[i as usize].0.as_bytes() < name);
    let len = order[start..]
        .iter()
        .take_while(|&&i| STATIC[i as usize].0.as_bytes() == name)
        .count();
    &order[start..start + len]
}

/// Writes a string literal with an `n`-bit length prefix, Huffman coded
/// when that is shorter. `first` holds the bits above the Huffman flag.
fn write_string(out: &mut Vec<u8>, first: u8, n: u8, s: &[u8]) {
    let coded = huffman::encoded_len(s);
    if coded < s.len() {
        write_int(out, first | (1 << n), n, coded);
        huffman::encode(s, out);
    } else {
        write_int(out, first, n, s.len());
        out.extend_from_slice(s);
    }
}

/// Encodes the field sections we send: static references and literals
/// only, so it has no state and a section never depends on another.
pub(crate) struct Encoder;

impl Encoder {
    /// Starts a field section: Required Insert Count 0, Base 0.
    pub(crate) fn begin(out: &mut Vec<u8>) {
        out.extend_from_slice(&[0, 0]);
    }

    /// Encodes `:status`.
    pub(crate) fn status(status: u16, out: &mut Vec<u8>) {
        let digits = [
            b'0' + (status / 100 % 10) as u8,
            b'0' + (status / 10 % 10) as u8,
            b'0' + (status % 10) as u8,
        ];
        Self::field(b":status", &digits, Index::No, out);
    }

    /// Encodes one field line. `name` must be lowercase. `Index::Never`
    /// marks a literal that intermediaries must not put in any table
    /// (section 7.1.3); the other two are the same here.
    pub(crate) fn field(name: &[u8], value: &[u8], index: Index, out: &mut Vec<u8>) {
        let named = static_named(name);
        let never = index == Index::Never;
        if !never {
            if let Some(&i) = named
                .iter()
                .find(|&&i| STATIC[i as usize].1.as_bytes() == value)
            {
                // Indexed field line, static: 1 1 index(6).
                write_int(out, 0xc0, 6, usize::from(i));
                return;
            }
        }
        let n = if never { 0x20 } else { 0 };
        match named.first() {
            // Literal with static name reference: 01 N 1 index(4).
            Some(&i) => write_int(out, 0x50 | n, 4, usize::from(i)),
            // Literal with literal name: 001 N H length(3).
            None => write_string(out, 0x20 | (n >> 1), 3, name),
        }
        write_string(out, 0, 7, value);
    }
}

/// Checks what the peer's encoder stream says (RFC 9204, section 4.3),
/// from after the stream type on. Returns how many bytes of `buf` were
/// read: whole instructions only, so the caller keeps the rest and calls
/// again with more.
///
/// The peer's encoder was told it has no dynamic table to fill
/// (`SETTINGS_QPACK_MAX_TABLE_CAPACITY` = 0): setting the capacity to zero
/// is the only thing it may say. Anything else is a connection error of
/// type `QPACK_ENCODER_STREAM_ERROR`.
pub(crate) fn encoder_stream(buf: &[u8]) -> Result<usize, Error> {
    let bad = |what| Error::new(Code::QpackEncoderStreamError, what);
    let mut done = 0;
    while let Some(&b) = buf.get(done) {
        // Set Dynamic Table Capacity: 001 capacity(5).
        if b & 0xe0 != 0x20 {
            return Err(bad("an insertion into a dynamic table of capacity zero"));
        }
        let mut pos = done;
        match read_int(buf, &mut pos, 5, bad("capacity too large"))? {
            Some(0) => done = pos,
            Some(_) => return Err(bad("a capacity above the one we allow, which is zero")),
            None => break,
        }
    }
    Ok(done)
}

/// Checks what the peer's decoder stream says (RFC 9204, section 4.4),
/// like [`encoder_stream`].
///
/// Our encoder never inserts into a dynamic table: the peer's decoder may
/// say that it gave up on a stream (Stream Cancellation), and nothing
/// else. Acknowledging a section that used no dynamic entry, or inserts
/// that were never made, is a connection error of type
/// `QPACK_DECODER_STREAM_ERROR`.
pub(crate) fn decoder_stream(buf: &[u8]) -> Result<usize, Error> {
    let bad = |what| Error::new(Code::QpackDecoderStreamError, what);
    let mut done = 0;
    while let Some(&b) = buf.get(done) {
        if b & 0x80 != 0 {
            return Err(bad("acknowledgment of a section without dynamic entries"));
        }
        if b & 0x40 == 0 {
            return Err(bad("insert count increment without any insertion"));
        }
        // Stream Cancellation: 01 stream id(6).
        let mut pos = done;
        match read_int(buf, &mut pos, 6, bad("stream id too large"))? {
            Some(_) => done = pos,
            None => break,
        }
    }
    Ok(done)
}

/// What QPACK must guarantee for any input, for the fuzz target and the
/// randomised tests: `block` either fails or decodes to fields that our
/// encoder turns into a section decoding back to the same fields; and the
/// two stream checkers never read past what they report. Returns `true`
/// if `block` decoded.
#[cfg(any(test, fuzzing))]
pub(crate) fn check(block: &[u8]) -> bool {
    for stream in [encoder_stream, decoder_stream] {
        if let Ok(used) = stream(block) {
            assert!(used <= block.len());
            // What was read is whole instructions: reading it alone gives
            // the same answer.
            assert_eq!(stream(&block[..used]), Ok(used));
        }
    }
    let mut fields = Vec::new();
    if decode(block, |n, v| fields.push((n.to_vec(), v.to_vec()))).is_err() {
        return false;
    }
    let mut out = Vec::new();
    Encoder::begin(&mut out);
    for (i, (n, v)) in fields.iter().enumerate() {
        let index = [Index::Yes, Index::No, Index::Never][(i + block.len()) % 3];
        Encoder::field(n, v, index, &mut out);
    }
    let mut back = Vec::new();
    decode(&out, |n, v| back.push((n.to_vec(), v.to_vec()))).expect("a section of ours decodes");
    assert_eq!(back, fields);
    true
}
