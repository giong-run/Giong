//! The protobuf wire format: varints, tags, and the reader and writer the
//! generated code is built on.
//!
//! Generated code calls these; applications rarely need them.

use crate::error::{Error, ErrorKind};

/// How deep messages and groups may nest before decoding stops with
/// `GIONG-E4006`. The same limit as Google's C++ library and prost.
pub const RECURSION_LIMIT: u32 = 100;

/// The largest field number protobuf allows.
pub const MAX_FIELD: u32 = (1 << 29) - 1;

/// The type of a field's encoding, the low three bits of its tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WireType {
    /// int32, int64, uint32, uint64, sint32, sint64, bool, enum.
    Varint = 0,
    /// fixed64, sfixed64, double.
    I64 = 1,
    /// string, bytes, messages, packed repeated fields.
    Len = 2,
    /// Start of a group (proto2).
    StartGroup = 3,
    /// End of a group (proto2).
    EndGroup = 4,
    /// fixed32, sfixed32, float.
    I32 = 5,
}

impl WireType {
    #[inline(always)]
    fn from_bits(bits: u32) -> Option<WireType> {
        Some(match bits {
            0 => WireType::Varint,
            1 => WireType::I64,
            2 => WireType::Len,
            3 => WireType::StartGroup,
            4 => WireType::EndGroup,
            5 => WireType::I32,
            _ => return None,
        })
    }
}

/// Reads protobuf fields from a byte slice without copying.
///
/// Every slice it returns borrows from the input, for the input's lifetime.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    field: u32,
}

impl<'a> Reader<'a> {
    /// A reader at the start of `buf`.
    #[inline(always)]
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader {
            buf,
            pos: 0,
            field: 0,
        }
    }

    /// The offset of the next byte to read.
    #[inline(always)]
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Whether every byte has been read.
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// The input this reader reads.
    #[inline(always)]
    pub fn buf(&self) -> &'a [u8] {
        self.buf
    }

    /// An error of this kind at the current offset and field.
    #[cold]
    pub fn error(&self, kind: ErrorKind) -> Error {
        Error::new(kind, self.pos, self.field)
    }

    /// Reads a varint.
    #[inline(always)]
    pub fn varint(&mut self) -> Result<u64, Error> {
        match self.buf.get(self.pos) {
            Some(&b) if b < 0x80 => {
                self.pos += 1;
                Ok(b as u64)
            }
            _ => self.varint_slow(),
        }
    }

    #[inline(never)]
    fn varint_slow(&mut self) -> Result<u64, Error> {
        let mut value: u64 = 0;
        let mut i = 0;
        loop {
            let Some(&b) = self.buf.get(self.pos + i) else {
                return Err(Error::new(ErrorKind::Truncated, self.pos + i, self.field));
            };
            if i == 9 {
                // The tenth byte may only hold the top bit of a u64.
                if b > 1 {
                    return Err(Error::new(ErrorKind::BadVarint, self.pos, self.field));
                }
                value |= (b as u64) << 63;
                self.pos += 10;
                return Ok(value);
            }
            value |= ((b & 0x7f) as u64) << (7 * i);
            i += 1;
            if b < 0x80 {
                self.pos += i;
                return Ok(value);
            }
        }
    }

    /// Reads the next field tag, or `None` at the end of the input.
    #[inline(always)]
    pub fn tag(&mut self) -> Result<Option<(u32, WireType)>, Error> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let start = self.pos;
        let raw = self.varint()?;
        // A tag is a 32-bit varint: at most 5 bytes.
        if self.pos - start > 5 {
            return Err(Error::new(ErrorKind::BadTag, start, 0));
        }
        let field = (raw >> 3) as u32;
        let wire = WireType::from_bits((raw & 7) as u32);
        match wire {
            Some(wire) if field != 0 && raw >> 3 <= MAX_FIELD as u64 => {
                self.field = field;
                Ok(Some((field, wire)))
            }
            _ => Err(Error::new(ErrorKind::BadTag, start, 0)),
        }
    }

    /// Reads 4 bytes, little-endian.
    #[inline(always)]
    pub fn fixed32(&mut self) -> Result<u32, Error> {
        match self.buf.get(self.pos..self.pos + 4) {
            Some(b) => {
                self.pos += 4;
                Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            }
            None => Err(self.error(ErrorKind::Truncated)),
        }
    }

    /// Reads 8 bytes, little-endian.
    #[inline(always)]
    pub fn fixed64(&mut self) -> Result<u64, Error> {
        match self.buf.get(self.pos..self.pos + 8) {
            Some(b) => {
                self.pos += 8;
                Ok(u64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ]))
            }
            None => Err(self.error(ErrorKind::Truncated)),
        }
    }

    /// Reads a length-delimited field and returns its bytes.
    #[inline(always)]
    pub fn bytes(&mut self) -> Result<&'a [u8], Error> {
        let len = self.varint()?;
        let rest = self.buf.len() - self.pos;
        if len > rest as u64 {
            return Err(self.error(ErrorKind::Truncated));
        }
        let start = self.pos;
        self.pos += len as usize;
        Ok(&self.buf[start..self.pos])
    }

    /// Reads a length-delimited field that must be UTF-8.
    #[inline(always)]
    pub fn str(&mut self) -> Result<&'a str, Error> {
        let b = self.bytes()?;
        match utf8(b) {
            Some(s) => Ok(s),
            None => Err(self.error(ErrorKind::BadUtf8)),
        }
    }

    /// Reads the body of a group whose start tag (for `field`) was just
    /// read, and the end tag after it. Returns the body without its tags.
    pub fn group(&mut self, field: u32, depth: u32) -> Result<&'a [u8], Error> {
        if depth == 0 {
            return Err(self.error(ErrorKind::TooDeep));
        }
        let start = self.pos;
        loop {
            let before = self.pos;
            match self.tag()? {
                None => return Err(Error::new(ErrorKind::BadGroup, self.pos, field)),
                Some((f, WireType::EndGroup)) => {
                    if f != field {
                        return Err(Error::new(ErrorKind::BadGroup, before, field));
                    }
                    self.field = field;
                    return Ok(&self.buf[start..before]);
                }
                Some((f, w)) => self.skip(f, w, depth - 1)?,
            }
        }
    }

    /// Skips the value of a field whose tag was just read.
    #[inline]
    pub fn skip(&mut self, field: u32, wire: WireType, depth: u32) -> Result<(), Error> {
        match wire {
            WireType::Varint => {
                self.varint()?;
            }
            WireType::I64 => {
                self.fixed64()?;
            }
            WireType::I32 => {
                self.fixed32()?;
            }
            WireType::Len => {
                self.bytes()?;
            }
            WireType::StartGroup => {
                self.group(field, depth)?;
            }
            WireType::EndGroup => {
                return Err(self.error(ErrorKind::BadGroup));
            }
        }
        Ok(())
    }

    /// Reads a packed run of varints and returns how many there were.
    #[inline]
    pub fn packed_varints(&mut self) -> Result<(&'a [u8], u32), Error> {
        let body = self.bytes()?;
        // Each varint ends in a byte with the top bit clear.
        let mut n = 0u32;
        for &b in body {
            n += (b < 0x80) as u32;
        }
        if let Some(&last) = body.last() {
            if last >= 0x80 {
                return Err(self.error(ErrorKind::BadPacked));
            }
        }
        // Reject over-long varints (more than 10 bytes, or a bad 10th byte).
        let mut r = Reader::new(body);
        if body.len() > n as usize {
            while !r.is_empty() {
                if r.varint().is_err() {
                    return Err(self.error(ErrorKind::BadPacked));
                }
            }
        }
        Ok((body, n))
    }

    /// Reads a packed run of fixed-width values of `width` bytes and
    /// returns how many there were.
    #[inline]
    pub fn packed_fixed(&mut self, width: usize) -> Result<(&'a [u8], u32), Error> {
        let body = self.bytes()?;
        if body.len() % width != 0 {
            return Err(self.error(ErrorKind::BadPacked));
        }
        Ok((body, (body.len() / width) as u32))
    }
}

/// `b` as a `&str`, or `None` when it is not UTF-8.
///
/// Most protobuf strings are short and ASCII, where the standard library's
/// check spends more time getting started than checking; this tests eight
/// bytes at a time for the high bit first and calls the standard check only
/// for strings that hold non-ASCII bytes.
#[inline(always)]
pub fn utf8(b: &[u8]) -> Option<&str> {
    let mut chunks = b.chunks_exact(8);
    let mut high = 0u64;
    for c in &mut chunks {
        high |= u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
    }
    for &x in chunks.remainder() {
        high |= x as u64;
    }
    if high & 0x8080_8080_8080_8080 == 0 {
        // SAFETY: every byte is below 0x80, and ASCII is valid UTF-8.
        Some(unsafe { std::str::from_utf8_unchecked(b) })
    } else {
        std::str::from_utf8(b).ok()
    }
}

/// Zigzag-decodes a 32-bit `sint32`.
#[inline(always)]
pub fn unzigzag32(v: u64) -> i32 {
    let v = v as u32;
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

/// Zigzag-decodes a 64-bit `sint64`.
#[inline(always)]
pub fn unzigzag64(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Zigzag-encodes a `sint32`.
#[inline(always)]
pub fn zigzag32(v: i32) -> u64 {
    ((v << 1) ^ (v >> 31)) as u32 as u64
}

/// Zigzag-encodes a `sint64`.
#[inline(always)]
pub fn zigzag64(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

/// How many bytes `v` takes as a varint.
#[inline(always)]
pub const fn varint_len(v: u64) -> usize {
    // 1 byte per 7 bits, at least 1.
    let bits = 64 - (v | 1).leading_zeros() as usize;
    (bits * 9 + 64) / 64
}

/// How many bytes the tag of `field` takes.
#[inline(always)]
pub const fn tag_len(field: u32) -> usize {
    varint_len((field as u64) << 3)
}

/// How many bytes a length-delimited field of `len` bytes takes, tag
/// excluded.
#[inline(always)]
pub const fn len_delimited(len: usize) -> usize {
    varint_len(len as u64) + len
}

/// Appends a varint.
#[inline(always)]
pub fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        if v < 0x80 {
            buf.push(v as u8);
            return;
        }
        buf.push((v as u8) | 0x80);
        v >>= 7;
    }
}

/// Appends a field tag.
#[inline(always)]
pub fn put_tag(buf: &mut Vec<u8>, field: u32, wire: WireType) {
    put_varint(buf, ((field as u64) << 3) | wire as u64);
}

/// Appends a length-delimited field: tag, length, bytes.
#[inline(always)]
pub fn put_bytes(buf: &mut Vec<u8>, field: u32, bytes: &[u8]) {
    put_tag(buf, field, WireType::Len);
    put_varint(buf, bytes.len() as u64);
    buf.extend_from_slice(bytes);
}

/// Appends 4 bytes, little-endian.
#[inline(always)]
pub fn put_fixed32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Appends 8 bytes, little-endian.
#[inline(always)]
pub fn put_fixed64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_round_trip() {
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(buf.len(), varint_len(v), "{v}");
            let mut r = Reader::new(&buf);
            assert_eq!(r.varint().unwrap(), v);
            assert!(r.is_empty());
        }
    }

    #[test]
    fn varint_limits() {
        // 11 bytes.
        let long = [
            0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ];
        assert_eq!(
            Reader::new(&long).varint().unwrap_err().kind(),
            ErrorKind::BadVarint
        );
        // Tenth byte holds more than the top bit.
        let over = [0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02];
        assert_eq!(
            Reader::new(&over).varint().unwrap_err().kind(),
            ErrorKind::BadVarint
        );
        let cut = [0x80u8, 0x80];
        assert_eq!(
            Reader::new(&cut).varint().unwrap_err().kind(),
            ErrorKind::Truncated
        );
    }

    #[test]
    fn tags() {
        assert_eq!(
            Reader::new(&[0x00]).tag().unwrap_err().kind(),
            ErrorKind::BadTag
        );
        assert_eq!(
            Reader::new(&[0x0e]).tag().unwrap_err().kind(),
            ErrorKind::BadTag
        );
        assert_eq!(
            Reader::new(&[0x08]).tag().unwrap(),
            Some((1, WireType::Varint))
        );
        assert_eq!(Reader::new(&[]).tag().unwrap(), None);
        // Field 1 in a 9-byte varint: too long for a tag.
        let long = [0x88, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        assert_eq!(
            Reader::new(&long).tag().unwrap_err().kind(),
            ErrorKind::BadTag
        );
    }

    #[test]
    fn utf8_check() {
        assert_eq!(
            utf8(b"plain ascii, longer than eight"),
            Some("plain ascii, longer than eight")
        );
        assert_eq!(utf8(b""), Some(""));
        assert_eq!(utf8("Nguyễn Văn".as_bytes()), Some("Nguyễn Văn"));
        assert_eq!(utf8(b"abcdefg\xff"), None);
        assert_eq!(utf8(b"abcdefgh\xc3"), None);
        assert_eq!(utf8(b"\xed\xa0\x80"), None); // a surrogate
        for len in 0..40 {
            let mut v = vec![b'a'; len];
            assert!(utf8(&v).is_some());
            if len > 0 {
                v[len - 1] = 0x80;
                assert!(utf8(&v).is_none(), "{len}");
            }
        }
    }

    #[test]
    fn zigzag() {
        for v in [0i32, -1, 1, i32::MIN, i32::MAX] {
            assert_eq!(unzigzag32(zigzag32(v)), v);
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX] {
            assert_eq!(unzigzag64(zigzag64(v)), v);
        }
    }

    #[test]
    fn groups() {
        // field 1 start group, field 2 varint 5, field 1 end group.
        let buf = [0x0b, 0x10, 0x05, 0x0c];
        let mut r = Reader::new(&buf);
        assert_eq!(r.tag().unwrap(), Some((1, WireType::StartGroup)));
        assert_eq!(r.group(1, 10).unwrap(), &[0x10, 0x05]);
        assert!(r.is_empty());
        // Mismatched end.
        let bad = [0x0b, 0x14];
        let mut r = Reader::new(&bad);
        r.tag().unwrap();
        assert_eq!(r.group(1, 10).unwrap_err().kind(), ErrorKind::BadGroup);
    }
}
