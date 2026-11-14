//! The PostgreSQL wire protocol, version 3.0: encoding what the client sends
//! and taking apart what the server sends.

use crate::error::Error;
use crate::types::Type;

pub(crate) const PROTOCOL_3_0: i32 = 196_608;
const SSL_REQUEST: i32 = 80_877_103;
const CANCEL_REQUEST: i32 = 80_877_102;

/// The largest number of parameters a statement can have: the protocol counts
/// them in an unsigned 16-bit field.
pub(crate) const MAX_PARAMS: usize = u16::MAX as usize;

// ------------------------------------------------------------------ frontend

/// Appends a message with `tag`, whose body `body` writes; the length field is
/// filled in afterwards.
fn message(out: &mut Vec<u8>, tag: u8, body: impl FnOnce(&mut Vec<u8>)) -> Result<(), Error> {
    out.push(tag);
    framed(out, body)
}

/// Appends a length-prefixed body (the length counts itself).
fn framed(out: &mut Vec<u8>, body: impl FnOnce(&mut Vec<u8>)) -> Result<(), Error> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    body(out);
    let len = i32::try_from(out.len() - start)
        .map_err(|_| Error::conversion("a message to the server would be larger than 2 GiB"))?;
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// A NUL-terminated string. The caller has checked that it holds no NUL.
fn cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// Refuses text that the protocol cannot carry.
pub(crate) fn check_text(what: &str, s: &str) -> Result<(), Error> {
    if s.as_bytes().contains(&0) {
        return Err(Error::conversion(format!(
            "{what} contains a NUL byte, which PostgreSQL cannot store in text; \
             remove it, or use bytea and &[u8] for binary data"
        )));
    }
    Ok(())
}

pub(crate) fn startup(out: &mut Vec<u8>, params: &[(&str, &str)]) -> Result<(), Error> {
    for (name, value) in params {
        check_text("a connection parameter", name)?;
        check_text("a connection parameter", value)?;
    }
    framed(out, |out| {
        out.extend_from_slice(&PROTOCOL_3_0.to_be_bytes());
        for (name, value) in params {
            cstr(out, name);
            cstr(out, value);
        }
        out.push(0);
    })
}

pub(crate) fn ssl_request(out: &mut Vec<u8>) {
    out.extend_from_slice(&8i32.to_be_bytes());
    out.extend_from_slice(&SSL_REQUEST.to_be_bytes());
}

pub(crate) fn cancel_request(out: &mut Vec<u8>, process_id: i32, secret_key: i32) {
    out.extend_from_slice(&16i32.to_be_bytes());
    out.extend_from_slice(&CANCEL_REQUEST.to_be_bytes());
    out.extend_from_slice(&process_id.to_be_bytes());
    out.extend_from_slice(&secret_key.to_be_bytes());
}

/// `PasswordMessage`, which also carries SASL responses.
pub(crate) fn password(out: &mut Vec<u8>, data: &[u8]) -> Result<(), Error> {
    message(out, b'p', |out| out.extend_from_slice(data))
}

pub(crate) fn sasl_initial_response(
    out: &mut Vec<u8>,
    mechanism: &str,
    data: &[u8],
) -> Result<(), Error> {
    message(out, b'p', |out| {
        cstr(out, mechanism);
        out.extend_from_slice(&(data.len() as i32).to_be_bytes());
        out.extend_from_slice(data);
    })
}

/// A simple query: the text may hold several statements, and has no
/// parameters.
pub(crate) fn query(out: &mut Vec<u8>, sql: &str) -> Result<(), Error> {
    check_text("the SQL text", sql)?;
    message(out, b'Q', |out| cstr(out, sql))
}

/// `Parse` into the named statement `name`, letting the server infer every
/// parameter type.
pub(crate) fn parse(out: &mut Vec<u8>, name: &str, sql: &str) -> Result<(), Error> {
    check_text("the SQL text", sql)?;
    message(out, b'P', |out| {
        cstr(out, name);
        cstr(out, sql);
        out.extend_from_slice(&0i16.to_be_bytes());
    })
}

pub(crate) fn describe_statement(out: &mut Vec<u8>, name: &str) -> Result<(), Error> {
    message(out, b'D', |out| {
        out.push(b'S');
        cstr(out, name);
    })
}

/// `Execute` of the unnamed portal, all rows.
pub(crate) fn execute(out: &mut Vec<u8>) -> Result<(), Error> {
    message(out, b'E', |out| {
        out.push(0);
        out.extend_from_slice(&0i32.to_be_bytes());
    })
}

pub(crate) fn close_statement(out: &mut Vec<u8>, name: &str) -> Result<(), Error> {
    message(out, b'C', |out| {
        out.push(b'S');
        cstr(out, name);
    })
}

pub(crate) fn sync(out: &mut Vec<u8>) {
    out.extend_from_slice(&[b'S', 0, 0, 0, 4]);
}

pub(crate) fn terminate(out: &mut Vec<u8>) {
    out.extend_from_slice(&[b'X', 0, 0, 0, 4]);
}

/// How a parameter value was written by [`ToSql`](crate::ToSql).
///
/// A parameter of any type may be sent as text, which the server parses as
/// it would a literal; binary needs the type's binary format exactly. The
/// enum is complete: PostgreSQL's protocol has only these two formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoded {
    /// SQL `NULL`; nothing was written.
    Null,
    /// The bytes are in PostgreSQL's binary format for the parameter's type.
    Binary,
    /// The bytes are text that the server parses as the parameter's type, as
    /// it would a literal in the SQL.
    Text,
}

/// `Bind` of the named statement to the unnamed portal. `encode(i, out)`
/// appends parameter `i` and says how. `binary_results[i]` asks for column
/// `i` in binary.
pub(crate) fn bind(
    out: &mut Vec<u8>,
    statement: &str,
    params: usize,
    mut encode: impl FnMut(usize, &mut Vec<u8>) -> Result<Encoded, Error>,
    binary_results: impl ExactSizeIterator<Item = bool>,
) -> Result<(), Error> {
    debug_assert!(params <= MAX_PARAMS);
    out.push(b'B');
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    out.push(0); // the unnamed portal
    cstr(out, statement);
    // The format of each parameter is only known once it is encoded: reserve
    // the codes and fill them in as the values are written.
    out.extend_from_slice(&(params as u16).to_be_bytes());
    let formats = out.len();
    out.resize(formats + 2 * params, 0);
    out.extend_from_slice(&(params as u16).to_be_bytes());
    for i in 0..params {
        let at = out.len();
        out.extend_from_slice(&[0; 4]);
        let len = match encode(i, out)? {
            Encoded::Null => {
                out.truncate(at + 4);
                -1i32
            }
            how => {
                if how == Encoded::Binary {
                    out[formats + 2 * i + 1] = 1;
                }
                i32::try_from(out.len() - at - 4).map_err(|_| {
                    Error::conversion(format!("parameter ${} is larger than 2 GiB", i + 1))
                })?
            }
        };
        out[at..at + 4].copy_from_slice(&len.to_be_bytes());
    }
    out.extend_from_slice(&(binary_results.len() as u16).to_be_bytes());
    for binary in binary_results {
        out.extend_from_slice(&(binary as i16).to_be_bytes());
    }
    let len = i32::try_from(out.len() - start)
        .map_err(|_| Error::conversion("the parameters add up to more than 2 GiB"))?;
    out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

// ------------------------------------------------------------------- backend

/// A message from the server, borrowing from the read buffer.
#[derive(Debug, PartialEq)]
pub(crate) enum Backend<'a> {
    AuthenticationOk,
    AuthenticationCleartextPassword,
    AuthenticationMd5Password([u8; 4]),
    AuthenticationSasl(Vec<&'a str>),
    AuthenticationSaslContinue(&'a [u8]),
    AuthenticationSaslFinal(&'a [u8]),
    /// Kerberos, GSSAPI, SSPI, SCM credentials: the code.
    AuthenticationOther(i32),
    KeyData {
        process_id: i32,
        secret_key: i32,
    },
    BindComplete,
    CloseComplete,
    CommandComplete(&'a str),
    CopyData,
    CopyDone,
    CopyInResponse,
    CopyOutResponse,
    CopyBothResponse,
    /// The body of a `DataRow`: read it with [`DataRow`].
    DataRow(&'a [u8]),
    EmptyQueryResponse,
    /// The fields; read them with [`DbError::parse`](crate::DbError).
    ErrorResponse(&'a [u8]),
    NegotiateProtocolVersion,
    NoData,
    NoticeResponse,
    NotificationResponse,
    ParameterDescription(Vec<Type>),
    ParameterStatus {
        name: &'a str,
        value: &'a str,
    },
    ParseComplete,
    PortalSuspended,
    ReadyForQuery(u8),
    RowDescription(Vec<FieldDescription<'a>>),
}

/// One column of a `RowDescription`.
#[derive(Debug, PartialEq)]
pub(crate) struct FieldDescription<'a> {
    pub name: &'a str,
    pub table_oid: u32,
    pub column_id: i16,
    pub type_oid: u32,
}

/// The tag and total length of the message at the start of `buf`, if all of
/// its header is there.
pub(crate) fn frame(buf: &[u8]) -> Result<Option<(u8, usize)>, Error> {
    if buf.len() < 5 {
        return Ok(None);
    }
    let len = i32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
    if len < 4 {
        return Err(Error::protocol(format!(
            "the server sent a message ({:?}) with an invalid length {len}",
            buf[0] as char
        )));
    }
    Ok(Some((buf[0], len as usize + 1)))
}

/// A cursor over a message body.
struct Cursor<'a> {
    buf: &'a [u8],
    what: &'static str,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8], what: &'static str) -> Self {
        Self { buf, what }
    }

    fn bad(&self) -> Error {
        Error::protocol(format!("the server sent a malformed {} message", self.what))
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.buf.len() < n {
            return Err(self.bad());
        }
        let (head, tail) = self.buf.split_at(n);
        self.buf = tail;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }

    fn i16(&mut self) -> Result<i16, Error> {
        let b = self.bytes(2)?;
        Ok(i16::from_be_bytes([b[0], b[1]]))
    }

    fn i32(&mut self) -> Result<i32, Error> {
        let b = self.bytes(4)?;
        Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn cstr(&mut self) -> Result<&'a str, Error> {
        let end = self
            .buf
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| self.bad())?;
        let s = std::str::from_utf8(&self.buf[..end]).map_err(|_| self.bad())?;
        self.buf = &self.buf[end + 1..];
        Ok(s)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.buf)
    }

    fn end(&self) -> Result<(), Error> {
        if self.buf.is_empty() {
            Ok(())
        } else {
            Err(self.bad())
        }
    }
}

/// Takes apart one message: `tag` and its body (without the length).
pub(crate) fn parse_backend(tag: u8, body: &[u8]) -> Result<Backend<'_>, Error> {
    use Backend::*;
    let empty = |m: Backend<'static>, what| {
        if body.is_empty() {
            Ok(m)
        } else {
            Err(Cursor::new(body, what).bad())
        }
    };
    match tag {
        b'R' => {
            let mut c = Cursor::new(body, "authentication");
            let msg = match c.i32()? {
                0 => AuthenticationOk,
                3 => AuthenticationCleartextPassword,
                5 => {
                    let salt = c.bytes(4)?;
                    AuthenticationMd5Password([salt[0], salt[1], salt[2], salt[3]])
                }
                10 => {
                    let mut mechanisms = Vec::new();
                    loop {
                        let m = c.cstr()?;
                        if m.is_empty() {
                            break;
                        }
                        mechanisms.push(m);
                    }
                    AuthenticationSasl(mechanisms)
                }
                11 => return Ok(AuthenticationSaslContinue(c.rest())),
                12 => return Ok(AuthenticationSaslFinal(c.rest())),
                other => return Ok(AuthenticationOther(other)),
            };
            c.end()?;
            Ok(msg)
        }
        b'K' => {
            let mut c = Cursor::new(body, "BackendKeyData");
            let process_id = c.i32()?;
            let secret_key = c.i32()?;
            // Protocol 3.2 allows longer keys; 3.0, which this client asks
            // for, has exactly four bytes.
            c.end()?;
            Ok(KeyData {
                process_id,
                secret_key,
            })
        }
        b'2' => empty(BindComplete, "BindComplete"),
        b'3' => empty(CloseComplete, "CloseComplete"),
        b'C' => {
            let mut c = Cursor::new(body, "CommandComplete");
            let tag = c.cstr()?;
            c.end()?;
            Ok(CommandComplete(tag))
        }
        b'd' => Ok(CopyData),
        b'c' => empty(CopyDone, "CopyDone"),
        b'G' => Ok(CopyInResponse),
        b'H' => Ok(CopyOutResponse),
        b'W' => Ok(CopyBothResponse),
        b'D' => Ok(DataRow(body)),
        b'I' => empty(EmptyQueryResponse, "EmptyQueryResponse"),
        b'E' => Ok(ErrorResponse(body)),
        b'v' => Ok(NegotiateProtocolVersion),
        b'n' => empty(NoData, "NoData"),
        b'N' => Ok(NoticeResponse),
        b'A' => Ok(NotificationResponse),
        b't' => {
            let mut c = Cursor::new(body, "ParameterDescription");
            let n = c.i16()? as u16 as usize;
            let mut types = Vec::with_capacity(n);
            for _ in 0..n {
                types.push(Type::from_oid(c.i32()? as u32));
            }
            c.end()?;
            Ok(ParameterDescription(types))
        }
        b'S' => {
            let mut c = Cursor::new(body, "ParameterStatus");
            let name = c.cstr()?;
            let value = c.cstr()?;
            c.end()?;
            Ok(ParameterStatus { name, value })
        }
        b'1' => empty(ParseComplete, "ParseComplete"),
        b's' => empty(PortalSuspended, "PortalSuspended"),
        b'Z' => {
            let mut c = Cursor::new(body, "ReadyForQuery");
            let status = c.u8()?;
            c.end()?;
            if !matches!(status, b'I' | b'T' | b'E') {
                return Err(c.bad());
            }
            Ok(ReadyForQuery(status))
        }
        b'T' => {
            let mut c = Cursor::new(body, "RowDescription");
            let n = c.i16()? as u16 as usize;
            let mut fields = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let name = c.cstr()?;
                let table_oid = c.i32()? as u32;
                let column_id = c.i16()?;
                let type_oid = c.i32()? as u32;
                let _size = c.i16()?;
                let _modifier = c.i32()?;
                let _format = c.i16()?;
                fields.push(FieldDescription {
                    name,
                    table_oid,
                    column_id,
                    type_oid,
                });
            }
            c.end()?;
            Ok(RowDescription(fields))
        }
        other => Err(Error::protocol(format!(
            "the server sent a message of unknown type {:?}",
            other as char
        ))),
    }
}

/// Walks the values of a `DataRow` body, checking the column count against
/// `columns`: `value(Some(range))` gives where a value is in `body`,
/// `value(None)` a `NULL`.
pub(crate) fn data_row(
    body: &[u8],
    columns: usize,
    mut value: impl FnMut(Option<std::ops::Range<usize>>),
) -> Result<(), Error> {
    let mut c = Cursor::new(body, "DataRow");
    let n = c.i16()? as u16 as usize;
    if n != columns {
        return Err(Error::protocol(format!(
            "the server sent a row with {n} values for a statement with {columns} columns"
        )));
    }
    for _ in 0..n {
        let len = c.i32()?;
        if len == -1 {
            value(None);
            continue;
        }
        if len < 0 {
            return Err(c.bad());
        }
        let at = body.len() - c.buf.len();
        c.bytes(len as usize)?;
        value(Some(at..at + len as usize));
    }
    c.end()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_is_framed_with_the_protocol_version() {
        let mut out = Vec::new();
        startup(&mut out, &[("user", "app"), ("database", "db")]).unwrap();
        let len = i32::from_be_bytes([out[0], out[1], out[2], out[3]]) as usize;
        assert_eq!(len, out.len());
        assert_eq!(&out[4..8], &PROTOCOL_3_0.to_be_bytes());
        assert_eq!(&out[8..], b"user\0app\0database\0db\0\0");
    }

    #[test]
    fn text_with_a_nul_is_refused() {
        let mut out = Vec::new();
        let e = query(&mut out, "SELECT 1\0").unwrap_err();
        assert!(e.to_string().contains("NUL byte"), "{e}");
    }

    #[test]
    fn bind_fills_in_formats_and_lengths() {
        let mut out = Vec::new();
        bind(
            &mut out,
            "s1",
            3,
            |i, out| match i {
                0 => {
                    out.extend_from_slice(&7i32.to_be_bytes());
                    Ok(Encoded::Binary)
                }
                1 => Ok(Encoded::Null),
                _ => {
                    out.extend_from_slice(b"abc");
                    Ok(Encoded::Text)
                }
            },
            [true, false].into_iter(),
        )
        .unwrap();
        let mut expected = vec![b'B', 0, 0, 0, 0, 0, b's', b'1', 0];
        expected.extend_from_slice(&[0, 3, 0, 1, 0, 0, 0, 0]); // formats
        expected.extend_from_slice(&[0, 3]);
        expected.extend_from_slice(&[0, 0, 0, 4, 0, 0, 0, 7]);
        expected.extend_from_slice(&[0xff, 0xff, 0xff, 0xff]);
        expected.extend_from_slice(&[0, 0, 0, 3, b'a', b'b', b'c']);
        expected.extend_from_slice(&[0, 2, 0, 1, 0, 0]);
        let len = (expected.len() - 1) as i32;
        expected[1..5].copy_from_slice(&len.to_be_bytes());
        assert_eq!(out, expected);
    }

    #[test]
    fn messages_are_taken_apart() {
        assert_eq!(
            parse_backend(b'Z', b"I").unwrap(),
            Backend::ReadyForQuery(b'I')
        );
        assert!(parse_backend(b'Z', b"X").is_err());
        assert!(parse_backend(b'Z', b"II").is_err());
        assert_eq!(
            parse_backend(b'C', b"INSERT 0 3\0").unwrap(),
            Backend::CommandComplete("INSERT 0 3")
        );
        assert_eq!(
            parse_backend(b'R', b"\0\0\0\x0aSCRAM-SHA-256\0SCRAM-SHA-256-PLUS\0\0").unwrap(),
            Backend::AuthenticationSasl(vec!["SCRAM-SHA-256", "SCRAM-SHA-256-PLUS"])
        );
        assert_eq!(
            parse_backend(b't', &[0, 2, 0, 0, 0, 23, 0, 0, 0, 25]).unwrap(),
            Backend::ParameterDescription(vec![Type::INT4, Type::TEXT])
        );
        assert!(parse_backend(b'?', b"").is_err());
        assert!(parse_backend(b'1', b"x").is_err());
    }

    #[test]
    fn data_rows_are_checked() {
        let body = [0, 2, 0, 0, 0, 1, b'x', 0xff, 0xff, 0xff, 0xff];
        let mut values = Vec::new();
        data_row(&body, 2, |v| values.push(v)).unwrap();
        assert_eq!(values, vec![Some(6..7), None]);
        assert!(data_row(&body, 3, |_| {}).is_err());
        assert!(data_row(&[0, 1, 0, 0, 0, 9, b'x'], 1, |_| {}).is_err());
        assert!(data_row(&[0, 1, 0, 0, 0, 0, b'x'], 1, |_| {}).is_err());
        assert!(data_row(&[0, 1, 0xff, 0xff, 0xff, 0xfe], 1, |_| {}).is_err());
    }

    #[test]
    fn frames_need_a_sane_length() {
        assert_eq!(frame(b"Z\0\0").unwrap(), None);
        assert_eq!(frame(b"Z\0\0\0\x05I").unwrap(), Some((b'Z', 6)));
        assert!(frame(b"Z\0\0\0\x03").is_err());
        assert!(frame(b"Z\xff\xff\xff\xff").is_err());
    }
}
