//! The MySQL client/server protocol: packets, and the messages in them.
//!
//! Every message travels in packets of at most 16 MiB - 1 bytes, each with a
//! 3-byte length and a 1-byte sequence number; a longer message is split,
//! and ends with a packet shorter than the maximum. Each command starts at
//! sequence number 0 and its answer counts on from there.

use crate::error::Error;
use crate::types::ColumnType;

/// The largest payload one packet can carry.
pub(crate) const MAX_PAYLOAD: usize = 0xFF_FFFF;

/// Capability flags, as the server offers them and the client picks them.
pub(crate) mod cap {
    pub const LONG_PASSWORD: u32 = 1;
    pub const FOUND_ROWS: u32 = 1 << 1;
    pub const LONG_FLAG: u32 = 1 << 2;
    pub const CONNECT_WITH_DB: u32 = 1 << 3;
    pub const PROTOCOL_41: u32 = 1 << 9;
    pub const SSL: u32 = 1 << 11;
    pub const TRANSACTIONS: u32 = 1 << 13;
    pub const SECURE_CONNECTION: u32 = 1 << 15;
    pub const MULTI_STATEMENTS: u32 = 1 << 16;
    pub const MULTI_RESULTS: u32 = 1 << 17;
    pub const PS_MULTI_RESULTS: u32 = 1 << 18;
    pub const PLUGIN_AUTH: u32 = 1 << 19;
    pub const CONNECT_ATTRS: u32 = 1 << 20;
    pub const PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;
    pub const DEPRECATE_EOF: u32 = 1 << 24;
}

/// What the client asks for. The server must offer all of `REQUIRED`.
pub(crate) const REQUIRED: u32 = cap::PROTOCOL_41
    | cap::SECURE_CONNECTION
    | cap::PLUGIN_AUTH
    | cap::PLUGIN_AUTH_LENENC_CLIENT_DATA
    | cap::DEPRECATE_EOF
    | cap::TRANSACTIONS
    | cap::MULTI_RESULTS
    | cap::PS_MULTI_RESULTS;

/// Asked for when the server offers it.
pub(crate) const WANTED: u32 = cap::LONG_PASSWORD
    | cap::LONG_FLAG
    | cap::MULTI_STATEMENTS
    | cap::CONNECT_ATTRS
    // `execute` reports the rows an UPDATE matched, not only those it
    // changed, as PostgreSQL does.
    | cap::FOUND_ROWS;

/// Server status flags, in OK and EOF packets.
pub(crate) mod status {
    pub const IN_TRANS: u16 = 0x0001;
    pub const MORE_RESULTS_EXISTS: u16 = 0x0008;
}

/// Command bytes.
pub(crate) mod com {
    pub const QUIT: u8 = 0x01;
    pub const QUERY: u8 = 0x03;
    pub const STMT_PREPARE: u8 = 0x16;
    pub const STMT_EXECUTE: u8 = 0x17;
    pub const STMT_CLOSE: u8 = 0x19;
}

/// The collation the connection uses: `utf8mb4_0900_ai_ci`, MySQL 8's
/// default, so that comparisons with literals behave as in the server's own
/// tables.
pub(crate) const UTF8MB4_0900_AI_CI: u8 = 255;

/// The collation number MySQL gives binary data (`BLOB`, `VARBINARY`).
pub(crate) const BINARY_COLLATION: u16 = 63;

/// The largest packet this client accepts, as it tells the server.
pub(crate) const MAX_PACKET: u32 = 1 << 30;

// ------------------------------------------------------------- writing

/// Starts a packet in `out`: room for the header. Finish it with
/// [`finish`].
pub(crate) fn begin(out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    start
}

/// Writes the header of the packet begun at `start`, splitting a payload of
/// 16 MiB or more into several packets. Returns the sequence number the
/// answer starts with.
pub(crate) fn finish(out: &mut Vec<u8>, start: usize, seq: u8) -> u8 {
    let len = out.len() - start - 4;
    if len < MAX_PAYLOAD {
        out[start..start + 3].copy_from_slice(&(len as u32).to_le_bytes()[..3]);
        out[start + 3] = seq;
        return seq.wrapping_add(1);
    }
    let payload = out.split_off(start + 4);
    out.truncate(start);
    let mut seq = seq;
    let mut chunks = payload.chunks(MAX_PAYLOAD).peekable();
    while let Some(chunk) = chunks.next() {
        out.extend_from_slice(&(chunk.len() as u32).to_le_bytes()[..3]);
        out.push(seq);
        out.extend_from_slice(chunk);
        seq = seq.wrapping_add(1);
        // A payload that is a whole number of full packets ends with an
        // empty one.
        if chunks.peek().is_none() && chunk.len() == MAX_PAYLOAD {
            out.extend_from_slice(&[0, 0, 0, seq]);
            seq = seq.wrapping_add(1);
        }
    }
    seq
}

/// A whole packet with `seq` and the payload `body` writes.
pub(crate) fn packet(out: &mut Vec<u8>, seq: u8, body: impl FnOnce(&mut Vec<u8>)) -> u8 {
    let start = begin(out);
    body(out);
    finish(out, start, seq)
}

/// A length-encoded integer.
pub(crate) fn put_lenenc(out: &mut Vec<u8>, n: u64) {
    match n {
        0..=250 => out.push(n as u8),
        251..=0xFFFF => {
            out.push(0xFC);
            out.extend_from_slice(&(n as u16).to_le_bytes());
        }
        0x1_0000..=0xFF_FFFF => {
            out.push(0xFD);
            out.extend_from_slice(&(n as u32).to_le_bytes()[..3]);
        }
        _ => {
            out.push(0xFE);
            out.extend_from_slice(&n.to_le_bytes());
        }
    }
}

/// A length-encoded string.
pub(crate) fn put_lenenc_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_lenenc(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A command with a text argument: `COM_QUERY`, `COM_STMT_PREPARE`.
/// Returns the sequence number of its answer.
pub(crate) fn command_text(out: &mut Vec<u8>, command: u8, text: &str) -> u8 {
    packet(out, 0, |out| {
        out.push(command);
        out.extend_from_slice(text.as_bytes());
    })
}

/// A command with a statement id: `COM_STMT_CLOSE`.
pub(crate) fn command_stmt(out: &mut Vec<u8>, command: u8, id: u32) {
    packet(out, 0, |out| {
        out.push(command);
        out.extend_from_slice(&id.to_le_bytes());
    });
}

/// A command with no argument, such as `COM_QUIT`.
pub(crate) fn command(out: &mut Vec<u8>, command: u8) -> u8 {
    packet(out, 0, |out| out.push(command))
}

// ------------------------------------------------------------- reading

/// Reads values off the front of a payload.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
}

fn short(what: &str) -> Error {
    Error::protocol(format!("a packet ended in the middle of {what}"))
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    pub(crate) fn rest(&self) -> &'a [u8] {
        self.buf
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub(crate) fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8], Error> {
        if self.buf.len() < n {
            return Err(short(what));
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    pub(crate) fn u8(&mut self, what: &str) -> Result<u8, Error> {
        Ok(self.take(1, what)?[0])
    }

    pub(crate) fn u16(&mut self, what: &str) -> Result<u16, Error> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub(crate) fn u32(&mut self, what: &str) -> Result<u32, Error> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub(crate) fn lenenc(&mut self, what: &str) -> Result<u64, Error> {
        Ok(match self.u8(what)? {
            n @ 0..=250 => u64::from(n),
            0xFC => u64::from(self.u16(what)?),
            0xFD => {
                let b = self.take(3, what)?;
                u64::from(u32::from_le_bytes([b[0], b[1], b[2], 0]))
            }
            0xFE => {
                let b = self.take(8, what)?;
                u64::from_le_bytes(b.try_into().expect("8 bytes"))
            }
            n => {
                return Err(Error::protocol(format!(
                    "{what} starts with 0x{n:02X}, which does not begin a length-encoded integer"
                )))
            }
        })
    }

    pub(crate) fn lenenc_bytes(&mut self, what: &str) -> Result<&'a [u8], Error> {
        let n = self.lenenc(what)?;
        let n = usize::try_from(n).map_err(|_| short(what))?;
        self.take(n, what)
    }

    /// A string ended by a NUL byte (the NUL is skipped).
    pub(crate) fn nul_str(&mut self, what: &str) -> Result<&'a [u8], Error> {
        let end = self
            .buf
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| short(what))?;
        let s = &self.buf[..end];
        self.buf = &self.buf[end + 1..];
        Ok(s)
    }
}

fn text(bytes: &[u8], what: &str) -> Result<String, Error> {
    String::from_utf8(bytes.to_vec())
        .map_err(|_| Error::protocol(format!("the server sent {what} that is not UTF-8")))
}

/// The server's first packet (`Protocol::HandshakeV10`).
#[derive(Debug)]
pub(crate) struct Handshake {
    pub server_version: String,
    pub connection_id: u32,
    pub capabilities: u32,
    pub nonce: Vec<u8>,
    pub plugin: String,
}

pub(crate) fn handshake(payload: &[u8]) -> Result<Handshake, Error> {
    let mut r = Reader::new(payload);
    match r.u8("the handshake")? {
        10 => {}
        0xFF => return Err(Error::protocol("the server refused the connection at once")),
        v => {
            return Err(Error::protocol(format!(
                "the server speaks protocol version {v}; giong-mysql speaks version 10 (MySQL 4.1 \
                 and later)"
            )))
        }
    }
    let server_version = text(r.nul_str("the server version")?, "a server version")?;
    let connection_id = r.u32("the connection id")?;
    let mut nonce = r.take(8, "the nonce")?.to_vec();
    r.u8("the handshake")?;
    let low = r.u16("the capabilities")?;
    let mut capabilities = u32::from(low);
    let mut plugin = String::from("mysql_native_password");
    if !r.is_empty() {
        r.u8("the character set")?;
        r.u16("the status")?;
        let high = r.u16("the capabilities")?;
        capabilities |= u32::from(high) << 16;
        let nonce_len = r.u8("the nonce length")?;
        r.take(10, "the handshake")?;
        if capabilities & cap::SECURE_CONNECTION != 0 {
            let n = (usize::from(nonce_len).saturating_sub(8)).max(13);
            let part = r.take(n, "the nonce")?;
            // The second part ends with a NUL that is not part of the nonce.
            let part = part.strip_suffix(&[0]).unwrap_or(part);
            nonce.extend_from_slice(part);
        }
        if capabilities & cap::PLUGIN_AUTH != 0 {
            // Some servers leave out the final NUL.
            let rest = r.rest();
            let name = rest.split(|&b| b == 0).next().unwrap_or(rest);
            plugin = text(name, "an authentication plugin name")?;
        }
    }
    Ok(Handshake {
        server_version,
        connection_id,
        capabilities,
        nonce,
        plugin,
    })
}

/// `Protocol::SSLRequest`: the start of a handshake response, before TLS.
pub(crate) fn ssl_request(out: &mut Vec<u8>, capabilities: u32) {
    packet(out, 1, |out| {
        out.extend_from_slice(&capabilities.to_le_bytes());
        out.extend_from_slice(&MAX_PACKET.to_le_bytes());
        out.push(UTF8MB4_0900_AI_CI);
        out.extend_from_slice(&[0; 23]);
    });
}

/// `Protocol::HandshakeResponse41`.
pub(crate) struct HandshakeResponse<'a> {
    pub capabilities: u32,
    pub user: &'a str,
    pub auth: &'a [u8],
    pub database: Option<&'a str>,
    pub plugin: &'a str,
    pub attrs: &'a [(&'a str, &'a str)],
}

/// Returns the sequence number of the server's answer.
pub(crate) fn handshake_response(out: &mut Vec<u8>, seq: u8, h: &HandshakeResponse) -> u8 {
    packet(out, seq, |out| {
        out.extend_from_slice(&h.capabilities.to_le_bytes());
        out.extend_from_slice(&MAX_PACKET.to_le_bytes());
        out.push(UTF8MB4_0900_AI_CI);
        out.extend_from_slice(&[0; 23]);
        out.extend_from_slice(h.user.as_bytes());
        out.push(0);
        put_lenenc_bytes(out, h.auth);
        if let Some(db) = h.database {
            out.extend_from_slice(db.as_bytes());
            out.push(0);
        }
        out.extend_from_slice(h.plugin.as_bytes());
        out.push(0);
        if h.capabilities & cap::CONNECT_ATTRS != 0 {
            let mut attrs = Vec::new();
            for (k, v) in h.attrs {
                put_lenenc_bytes(&mut attrs, k.as_bytes());
                put_lenenc_bytes(&mut attrs, v.as_bytes());
            }
            put_lenenc_bytes(out, &attrs);
        }
    })
}

/// An `OK_Packet` (header 0x00, or 0xFE where it ends a result set).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ok {
    pub affected_rows: u64,
    pub last_insert_id: u64,
    pub status: u16,
    pub warnings: u16,
}

pub(crate) fn ok(payload: &[u8]) -> Result<Ok, Error> {
    let mut r = Reader::new(payload);
    r.u8("an OK packet")?;
    Ok(Ok {
        affected_rows: r.lenenc("an OK packet")?,
        last_insert_id: r.lenenc("an OK packet")?,
        status: r.u16("an OK packet")?,
        warnings: r.u16("an OK packet")?,
    })
}

/// Whether a packet in a result set is the OK packet that ends it, rather
/// than a row: with `CLIENT_DEPRECATE_EOF` the end is an OK packet with the
/// header 0xFE, which a row cannot start with unless it is longer than any
/// such OK packet can be.
pub(crate) fn is_end_of_rows(payload: &[u8]) -> bool {
    payload.first() == Some(&0xFE) && payload.len() < MAX_PAYLOAD
}

/// An `ERR_Packet`.
pub(crate) struct Err<'a> {
    pub code: u16,
    pub sqlstate: &'a str,
    pub message: &'a str,
}

pub(crate) fn err(payload: &[u8]) -> Result<Err<'_>, Error> {
    let mut r = Reader::new(payload);
    r.u8("an error packet")?;
    let code = r.u16("an error packet")?;
    let mut sqlstate = "HY000";
    if r.rest().first() == Some(&b'#') {
        r.u8("an error packet")?;
        sqlstate = std::str::from_utf8(r.take(5, "an error packet")?)
            .map_err(|_| Error::protocol("the server sent a SQLSTATE that is not ASCII"))?;
    }
    let message = std::str::from_utf8(r.rest())
        .map_err(|_| Error::protocol("the server sent an error message that is not UTF-8"))?;
    Ok(Err {
        code,
        sqlstate,
        message,
    })
}

/// A column definition (`Protocol::ColumnDefinition41`), the parts this
/// crate uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnDef {
    pub name: Box<str>,
    pub collation: u16,
    pub length: u32,
    pub ty: ColumnType,
    pub flags: u16,
    pub decimals: u8,
}

pub(crate) fn column_def(payload: &[u8]) -> Result<ColumnDef, Error> {
    let mut r = Reader::new(payload);
    let what = "a column definition";
    for _ in 0..4 {
        // catalog, schema, table, original table
        r.lenenc_bytes(what)?;
    }
    let name = std::str::from_utf8(r.lenenc_bytes(what)?)
        .map_err(|_| Error::protocol("the server sent a column name that is not UTF-8"))?
        .into();
    r.lenenc_bytes(what)?; // original name
    r.lenenc(what)?; // length of the fixed fields, always 0x0C
    let collation = r.u16(what)?;
    let length = r.u32(what)?;
    let ty = ColumnType::from_code(r.u8(what)?);
    let flags = r.u16(what)?;
    let decimals = r.u8(what)?;
    Ok(ColumnDef {
        name,
        collation,
        length,
        ty,
        flags,
        decimals,
    })
}

/// `COM_STMT_PREPARE_OK`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PrepareOk {
    pub id: u32,
    pub columns: u16,
    pub params: u16,
}

pub(crate) fn prepare_ok(payload: &[u8]) -> Result<PrepareOk, Error> {
    let mut r = Reader::new(payload);
    let what = "the answer to a prepare";
    r.u8(what)?;
    let id = r.u32(what)?;
    let columns = r.u16(what)?;
    let params = r.u16(what)?;
    Ok(PrepareOk {
        id,
        columns,
        params,
    })
}

/// The 3-byte length and the sequence number at the front of a packet.
pub(crate) fn header(buf: &[u8]) -> Option<(usize, u8)> {
    let h = buf.get(..4)?;
    Some((u32::from_le_bytes([h[0], h[1], h[2], 0]) as usize, h[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenenc_round_trips() {
        for n in [
            0,
            1,
            250,
            251,
            0xFFFF,
            0x1_0000,
            0xFF_FFFF,
            0x100_0000,
            u64::MAX,
        ] {
            let mut out = Vec::new();
            put_lenenc(&mut out, n);
            let mut r = Reader::new(&out);
            assert_eq!(r.lenenc("n").unwrap(), n);
            assert!(r.is_empty());
        }
        assert!(Reader::new(&[0xFB]).lenenc("x").is_err());
        assert!(Reader::new(&[0xFC, 1]).lenenc("x").is_err());
    }

    #[test]
    fn long_payloads_are_split_into_packets() {
        let mut out = Vec::new();
        let start = begin(&mut out);
        out.resize(start + 4 + MAX_PAYLOAD + 10, 7);
        let next = finish(&mut out, start, 0);
        assert_eq!(next, 2);
        assert_eq!(header(&out), Some((MAX_PAYLOAD, 0)));
        let second = 4 + MAX_PAYLOAD;
        assert_eq!(header(&out[second..]), Some((10, 1)));
        assert_eq!(out.len(), second + 4 + 10);

        // Exactly one full packet: an empty one follows.
        let mut out = Vec::new();
        let start = begin(&mut out);
        out.resize(start + 4 + MAX_PAYLOAD, 1);
        assert_eq!(finish(&mut out, start, 0), 2);
        assert_eq!(header(&out[4 + MAX_PAYLOAD..]), Some((0, 1)));

        let mut out = Vec::new();
        assert_eq!(command_text(&mut out, com::QUERY, "SELECT 1"), 1);
        assert_eq!(&out[..5], &[9, 0, 0, 0, com::QUERY]);
    }

    #[test]
    fn a_mysql_8_handshake_is_read() {
        // Captured from MySQL 8.0 (the nonce bytes changed).
        let mut p = vec![10];
        p.extend_from_slice(b"8.0.46\0");
        p.extend_from_slice(&42u32.to_le_bytes());
        p.extend_from_slice(b"abcdefgh");
        p.push(0);
        p.extend_from_slice(&0xFFFFu16.to_le_bytes());
        p.push(255);
        p.extend_from_slice(&2u16.to_le_bytes());
        p.extend_from_slice(&0xDFFFu16.to_le_bytes());
        p.push(21);
        p.extend_from_slice(&[0; 10]);
        p.extend_from_slice(b"ijklmnopqrst\0");
        p.extend_from_slice(b"caching_sha2_password\0");
        let h = handshake(&p).unwrap();
        assert_eq!(h.server_version, "8.0.46");
        assert_eq!(h.connection_id, 42);
        assert_eq!(h.nonce, b"abcdefghijklmnopqrst");
        assert_eq!(h.plugin, "caching_sha2_password");
        assert_eq!(h.capabilities & REQUIRED, REQUIRED);
        assert!(handshake(&[9, b'x', 0]).is_err());
        assert!(handshake(&p[..20]).is_err());
    }

    #[test]
    fn ok_and_error_packets_are_read() {
        let o = ok(&[0, 3, 0xFC, 0x10, 0x27, 0x03, 0x00, 1, 0]).unwrap();
        assert_eq!(
            o,
            Ok {
                affected_rows: 3,
                last_insert_id: 10_000,
                status: 3,
                warnings: 1
            }
        );
        let mut p = vec![0xFF];
        p.extend_from_slice(&1146u16.to_le_bytes());
        p.extend_from_slice(b"#42S02Table 'shop.nope' doesn't exist");
        let e = err(&p).unwrap();
        assert_eq!((e.code, e.sqlstate), (1146, "42S02"));
        assert_eq!(e.message, "Table 'shop.nope' doesn't exist");
        assert!(is_end_of_rows(&[0xFE, 0, 0, 2, 0, 0, 0]));
        assert!(!is_end_of_rows(&[0x00, 0xFE]));
    }
}
