//! HPACK, the header compression of HTTP/2 (RFC 7541).
//!
//! [`Decoder`] reads the header blocks a peer sends, [`Encoder`] writes
//! ours. Each keeps the dynamic table of its direction. Both are strict:
//! anything RFC 7541 does not allow is a [`Error`], which HTTP/2 turns into
//! a connection error of type `COMPRESSION_ERROR`.

mod huffman;
mod huffman_codes;

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fmt;

/// The static table (RFC 7541, Appendix A): index 1 is the first entry.
#[rustfmt::skip]
const STATIC: [(&str, &str); 61] = [
    (":authority", ""), (":method", "GET"), (":method", "POST"), (":path", "/"),
    (":path", "/index.html"), (":scheme", "http"), (":scheme", "https"),
    (":status", "200"), (":status", "204"), (":status", "206"), (":status", "304"),
    (":status", "400"), (":status", "404"), (":status", "500"), ("accept-charset", ""),
    ("accept-encoding", "gzip, deflate"), ("accept-language", ""), ("accept-ranges", ""),
    ("accept", ""), ("access-control-allow-origin", ""), ("age", ""), ("allow", ""),
    ("authorization", ""), ("cache-control", ""), ("content-disposition", ""),
    ("content-encoding", ""), ("content-language", ""), ("content-length", ""),
    ("content-location", ""), ("content-range", ""), ("content-type", ""), ("cookie", ""),
    ("date", ""), ("etag", ""), ("expect", ""), ("expires", ""), ("from", ""), ("host", ""),
    ("if-match", ""), ("if-modified-since", ""), ("if-none-match", ""), ("if-range", ""),
    ("if-unmodified-since", ""), ("last-modified", ""), ("link", ""), ("location", ""),
    ("max-forwards", ""), ("proxy-authenticate", ""), ("proxy-authorization", ""),
    ("range", ""), ("referer", ""), ("refresh", ""), ("retry-after", ""), ("server", ""),
    ("set-cookie", ""), ("strict-transport-security", ""), ("transfer-encoding", ""),
    ("user-agent", ""), ("vary", ""), ("via", ""), ("www-authenticate", ""),
];

/// The default size of a dynamic table, and the largest one we keep in
/// either direction.
pub(crate) const TABLE_SIZE: usize = 4096;

/// Overhead counted for every table entry (RFC 7541, section 4.1).
const ENTRY_OVERHEAD: usize = 32;

/// A header block that breaks RFC 7541.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Error(pub(crate) &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// One entry: name and value in one allocation.
struct Entry {
    bytes: Box<[u8]>,
    name_len: usize,
}

impl Entry {
    fn name(&self) -> &[u8] {
        &self.bytes[..self.name_len]
    }

    fn value(&self) -> &[u8] {
        &self.bytes[self.name_len..]
    }

    fn size(&self) -> usize {
        self.bytes.len() + ENTRY_OVERHEAD
    }
}

/// A dynamic table: newest entry first.
struct Table {
    entries: VecDeque<Entry>,
    size: usize,
    max: usize,
}

impl Table {
    fn new(max: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            size: 0,
            max,
        }
    }

    /// Entry `index` of the dynamic table, 0 being the newest.
    fn get(&self, index: usize) -> Option<&Entry> {
        self.entries.get(index)
    }

    fn evict_to(&mut self, max: usize) {
        while self.size > max {
            let Some(old) = self.entries.pop_back() else {
                break;
            };
            self.size -= old.size();
        }
        if self.entries.is_empty() {
            // An empty table gives its memory back.
            self.entries = VecDeque::new();
        }
    }

    fn set_max(&mut self, max: usize) {
        self.max = max;
        self.evict_to(max);
    }

    fn insert(&mut self, name: &[u8], value: &[u8]) {
        let size = name.len() + value.len() + ENTRY_OVERHEAD;
        if size > self.max {
            // Too large for the table: it empties it (section 4.4).
            self.evict_to(0);
            return;
        }
        self.evict_to(self.max - size);
        let mut bytes = Vec::with_capacity(name.len() + value.len());
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(value);
        self.entries.push_front(Entry {
            bytes: bytes.into_boxed_slice(),
            name_len: name.len(),
        });
        self.size += size;
    }

    /// Bytes this table holds on the heap.
    fn heap_bytes(&self) -> usize {
        self.entries.capacity() * std::mem::size_of::<Entry>()
            + self.entries.iter().map(|e| e.bytes.len()).sum::<usize>()
    }

    /// The size kept matches the entries, within the limit.
    #[cfg(any(test, fuzzing))]
    fn check(&self) {
        assert_eq!(
            self.size,
            self.entries.iter().map(Entry::size).sum::<usize>()
        );
        assert!(self.size <= self.max, "{} > {}", self.size, self.max);
    }
}

/// A field from a table: `(name, value)`.
fn lookup(table: &Table, index: usize) -> Result<(&[u8], &[u8]), Error> {
    match index {
        0 => Err(Error("index 0 in a header block")),
        1..=61 => {
            let (name, value) = STATIC[index - 1];
            Ok((name.as_bytes(), value.as_bytes()))
        }
        _ => table
            .get(index - 62)
            .map(|e| (e.name(), e.value()))
            .ok_or(Error("index past the end of the dynamic table")),
    }
}

/// Reads an integer with an `n`-bit prefix (section 5.1) at `*pos`.
fn read_int(block: &[u8], pos: &mut usize, n: u8) -> Result<usize, Error> {
    let mask = ((1u16 << n) - 1) as u8;
    let first = *block
        .get(*pos)
        .ok_or(Error("header block ends in an integer"))?
        & mask;
    *pos += 1;
    if first < mask {
        return Ok(first as usize);
    }
    let mut value = u64::from(mask);
    let mut shift = 0;
    loop {
        let b = *block
            .get(*pos)
            .ok_or(Error("header block ends in an integer"))?;
        *pos += 1;
        value += u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift > 28 {
            return Err(Error("integer too large in a header block"));
        }
    }
    usize::try_from(value)
        .ok()
        .filter(|&v| v <= u32::MAX as usize)
        .ok_or(Error("integer too large in a header block"))
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

/// Writes a string literal, Huffman coded when that is shorter.
fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    let coded = huffman::encoded_len(s);
    if coded < s.len() {
        write_int(out, 0x80, 7, coded);
        huffman::encode(s, out);
    } else {
        write_int(out, 0, 7, s.len());
        out.extend_from_slice(s);
    }
}

/// Scratch space for Huffman-decoded strings, shared by the connections of a
/// worker so that an idle connection keeps none.
struct Scratch {
    name: Vec<u8>,
    value: Vec<u8>,
}

thread_local! {
    static SCRATCH: RefCell<Scratch> = const {
        RefCell::new(Scratch { name: Vec::new(), value: Vec::new() })
    };
}

/// Largest scratch buffer kept between header blocks.
const SCRATCH_KEEP: usize = 16 * 1024;

/// Which of the two scratch buffers a string goes to.
#[derive(Clone, Copy)]
enum Slot {
    Name,
    Value,
}

/// Where a string read from a block is: still in the block, or decoded
/// into its scratch buffer.
#[derive(Clone, Copy)]
enum Str {
    Block(usize, usize),
    Scratch,
}

/// Reads a string literal at `*pos`.
fn read_string(
    block: &[u8],
    pos: &mut usize,
    scratch: &mut Scratch,
    slot: Slot,
) -> Result<Str, Error> {
    let huffman = block
        .get(*pos)
        .ok_or(Error("header block ends in a string"))?
        & 0x80
        != 0;
    let len = read_int(block, pos, 7)?;
    let end = pos
        .checked_add(len)
        .filter(|&end| end <= block.len())
        .ok_or(Error("string longer than the header block"))?;
    let raw = &block[*pos..end];
    let start = *pos;
    *pos = end;
    if !huffman {
        return Ok(Str::Block(start, end));
    }
    let buf = match slot {
        Slot::Name => &mut scratch.name,
        Slot::Value => &mut scratch.value,
    };
    buf.clear();
    huffman::decode(raw, buf).map_err(|()| Error("invalid Huffman code in a string"))?;
    Ok(Str::Scratch)
}

/// Decodes the header blocks a peer sends.
pub(crate) struct Decoder {
    table: Table,
    /// The largest table the peer may ask for: what our
    /// `SETTINGS_HEADER_TABLE_SIZE` says.
    limit: usize,
}

impl Decoder {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            table: Table::new(limit),
            limit,
        }
    }

    /// Decodes one complete header block, calling `field` for each header
    /// field in order. The dynamic table is updated as the block says, so
    /// a block whose fields the caller ignores must still be decoded.
    pub(crate) fn decode(
        &mut self,
        block: &[u8],
        mut field: impl FnMut(&[u8], &[u8]),
    ) -> Result<(), Error> {
        SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            let scratch = &mut *scratch;
            let result = self.decode_with(block, scratch, &mut field);
            for buf in [&mut scratch.name, &mut scratch.value] {
                if buf.capacity() > SCRATCH_KEEP {
                    *buf = Vec::new();
                }
            }
            result
        })
    }

    fn decode_with(
        &mut self,
        block: &[u8],
        scratch: &mut Scratch,
        field: &mut impl FnMut(&[u8], &[u8]),
    ) -> Result<(), Error> {
        let mut pos = 0;
        let mut fields = false;
        while pos < block.len() {
            let b = block[pos];
            if b & 0x80 != 0 {
                // Indexed field (section 6.1).
                let index = read_int(block, &mut pos, 7)?;
                let (name, value) = lookup(&self.table, index)?;
                field(name, value);
                fields = true;
            } else if b & 0xe0 == 0x20 {
                // Dynamic table size update (section 6.3): only before the
                // first field of a block.
                if fields {
                    return Err(Error("table size update after a header field"));
                }
                let size = read_int(block, &mut pos, 5)?;
                if size > self.limit {
                    return Err(Error("table size update above the agreed limit"));
                }
                self.table.set_max(size);
            } else {
                // Literal field: with incremental indexing (01), without
                // indexing (0000) or never indexed (0001).
                let (prefix, index_it) = if b & 0xc0 == 0x40 {
                    (6, true)
                } else {
                    (4, false)
                };
                let index = read_int(block, &mut pos, prefix)?;
                let name = if index == 0 {
                    Some(read_string(block, &mut pos, scratch, Slot::Name)?)
                } else {
                    // Checked now, read below.
                    lookup(&self.table, index)?;
                    None
                };
                let value = read_string(block, &mut pos, scratch, Slot::Value)?;
                let value_bytes: &[u8] = match value {
                    Str::Block(start, end) => &block[start..end],
                    Str::Scratch => &scratch.value,
                };
                let name_bytes: &[u8] = match name {
                    Some(Str::Block(start, end)) => &block[start..end],
                    Some(Str::Scratch) => &scratch.name,
                    None => lookup(&self.table, index)?.0,
                };
                field(name_bytes, value_bytes);
                fields = true;
                if index_it {
                    // The name may be in the table that inserting changes:
                    // copy it out first.
                    let mut owned = std::mem::take(&mut scratch.name);
                    match name {
                        Some(Str::Scratch) => {}
                        Some(Str::Block(start, end)) => {
                            owned.clear();
                            owned.extend_from_slice(&block[start..end]);
                        }
                        None => {
                            owned.clear();
                            owned.extend_from_slice(lookup(&self.table, index)?.0);
                        }
                    }
                    let value_bytes: &[u8] = match value {
                        Str::Block(start, end) => &block[start..end],
                        Str::Scratch => &scratch.value,
                    };
                    self.table.insert(&owned, value_bytes);
                    scratch.name = owned;
                }
            }
        }
        Ok(())
    }

    /// Bytes the dynamic table holds on the heap.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.table.heap_bytes()
    }
}

/// How a field may be stored in the peer's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Index {
    /// Add it to the dynamic table, for the next blocks to refer to.
    Yes,
    /// Leave the table alone: values that change with every message.
    No,
    /// Leave it out of every table, intermediaries included: secrets.
    Never,
}

/// Encodes the header blocks we send.
pub(crate) struct Encoder {
    table: Table,
    /// Size updates to announce at the start of the next block: the
    /// smallest size the table went through, then its current size.
    pending: Option<(usize, usize)>,
}

impl Encoder {
    pub(crate) fn new() -> Self {
        Self {
            table: Table::new(TABLE_SIZE),
            pending: None,
        }
    }

    /// The peer's `SETTINGS_HEADER_TABLE_SIZE` changed to `limit`.
    pub(crate) fn set_limit(&mut self, limit: usize) {
        let max = limit.min(TABLE_SIZE);
        if max == self.table.max {
            return;
        }
        let smallest = self.pending.map_or(max, |(smallest, _)| smallest.min(max));
        self.pending = Some((smallest, max));
        self.table.set_max(max);
    }

    /// Starts a header block: announces table size changes first.
    pub(crate) fn begin(&mut self, out: &mut Vec<u8>) {
        if let Some((smallest, current)) = self.pending.take() {
            if smallest < current {
                write_int(out, 0x20, 5, smallest);
            }
            write_int(out, 0x20, 5, current);
        }
    }

    /// Encodes `:status`.
    pub(crate) fn status(&mut self, status: u16, out: &mut Vec<u8>) {
        let index = match status {
            200 => 8,
            204 => 9,
            206 => 10,
            304 => 11,
            400 => 12,
            404 => 13,
            500 => 14,
            _ => {
                let mut digits = [0u8; 3];
                digits[0] = b'0' + (status / 100 % 10) as u8;
                digits[1] = b'0' + (status / 10 % 10) as u8;
                digits[2] = b'0' + (status % 10) as u8;
                self.field(b":status", &digits, Index::Yes, out);
                return;
            }
        };
        write_int(out, 0x80, 7, index);
    }

    /// Encodes one field. `name` must be lowercase.
    pub(crate) fn field(&mut self, name: &[u8], value: &[u8], index: Index, out: &mut Vec<u8>) {
        let mut name_index = 0;
        for (i, &(n, v)) in STATIC.iter().enumerate() {
            if n.as_bytes() == name {
                if v.as_bytes() == value && index != Index::Never {
                    write_int(out, 0x80, 7, i + 1);
                    return;
                }
                if name_index == 0 {
                    name_index = i + 1;
                }
            }
        }
        if index != Index::Never {
            for (i, entry) in self.table.entries.iter().enumerate() {
                if entry.name() == name {
                    if entry.value() == value {
                        write_int(out, 0x80, 7, 62 + i);
                        return;
                    }
                    if name_index == 0 {
                        name_index = 62 + i;
                    }
                }
            }
        }
        let size = name.len() + value.len() + ENTRY_OVERHEAD;
        let index = if index == Index::Yes && size > self.table.max {
            Index::No
        } else {
            index
        };
        match index {
            Index::Yes => write_int(out, 0x40, 6, name_index),
            Index::No => write_int(out, 0x00, 4, name_index),
            Index::Never => write_int(out, 0x10, 4, name_index),
        }
        if name_index == 0 {
            write_string(out, name);
        }
        write_string(out, value);
        if index == Index::Yes {
            self.table.insert(name, value);
        }
    }

    /// Bytes the dynamic table holds on the heap.
    pub(crate) fn heap_bytes(&self) -> usize {
        self.table.heap_bytes()
    }
}

#[cfg(test)]
mod tests;

/// What HPACK must guarantee for any input, for the fuzz targets and the
/// randomised tests. `input`: a byte that picks the decoder's table size,
/// then header blocks, each after a byte giving its length. The decoder
/// keeps its table from block to block, as on a connection.
///
/// Every block either fails or decodes to fields that our encoder turns
/// into a block decoding back to the same fields; the tables never grow
/// past their limits. Returns `true` if a block decoded.
#[cfg(any(test, fuzzing))]
pub(crate) fn check(input: &[u8]) -> bool {
    let Some((&first, mut rest)) = input.split_first() else {
        return false;
    };
    let limit = [0, 1, 64, 256, TABLE_SIZE][first as usize % 5];
    let mut decoder = Decoder::new(limit);
    let mut encoder = Encoder::new();
    let mut again = Decoder::new(TABLE_SIZE);
    let mut decoded = false;
    while let Some((&len, tail)) = rest.split_first() {
        let (block, tail) = tail.split_at((len as usize).min(tail.len()));
        rest = tail;
        let mut fields = Vec::new();
        let result = decoder.decode(block, |n, v| fields.push((n.to_vec(), v.to_vec())));
        decoder.table.check();
        assert!(decoder.table.max <= limit);
        if result.is_err() {
            break;
        }
        decoded = true;
        if len % 7 == 0 {
            // The peer shrinks or grows our table now and then.
            encoder.set_limit([0, 100, TABLE_SIZE][len as usize % 3]);
        }
        let mut out = Vec::new();
        encoder.begin(&mut out);
        for (i, (n, v)) in fields.iter().enumerate() {
            let index = [Index::Yes, Index::No, Index::Never][(i + len as usize) % 3];
            encoder.field(n, v, index, &mut out);
        }
        encoder.table.check();
        let mut back = Vec::new();
        again
            .decode(&out, |n, v| back.push((n.to_vec(), v.to_vec())))
            .expect("a block of ours decodes");
        again.table.check();
        assert_eq!(back, fields);
    }
    decoded
}

/// Huffman coding: anything encodes and decodes back; any input decodes
/// or fails, and what decodes encodes back to the same input (the padding
/// allowed is unique). Returns `true` if `input` decoded.
#[cfg(any(test, fuzzing))]
pub(crate) fn check_huffman(input: &[u8]) -> bool {
    let mut coded = Vec::new();
    huffman::encode(input, &mut coded);
    assert_eq!(coded.len(), huffman::encoded_len(input));
    let mut back = Vec::new();
    huffman::decode(&coded, &mut back).expect("our own coding decodes");
    assert_eq!(back, input);

    let mut decoded = Vec::new();
    if huffman::decode(input, &mut decoded).is_err() {
        return false;
    }
    let mut again = Vec::new();
    huffman::encode(&decoded, &mut again);
    assert_eq!(again, input);
    true
}
