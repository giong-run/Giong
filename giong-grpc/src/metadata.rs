//! Metadata: the named values sent with a call, before the messages and
//! after them.

use std::fmt;

use giong::error::{Code as ErrorCode, Error};

/// Names a call's own machinery uses, which are not metadata: they are
/// refused when metadata is built and left out when it is received.
const RESERVED: [&str; 13] = [
    "accept-encoding",
    "connection",
    "content-encoding",
    "content-length",
    "content-type",
    "date",
    "host",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The named values sent with a call: by the client before the request, by
/// the server before the response and after it.
///
/// A name is lowercase letters, digits, `-`, `_` and `.`. A name that ends
/// in `-bin` holds any bytes (sent as base64); any other holds printable
/// ASCII text. A name may appear more than once, and the order is kept.
///
/// ```
/// use giong_grpc::Metadata;
///
/// let mut metadata = Metadata::new();
/// metadata.add("x-request-id", "7f3a")?;
/// metadata.add("trace-bin", [0u8, 159, 255])?;
/// assert_eq!(metadata.get_str("x-request-id"), Some("7f3a"));
/// assert_eq!(metadata.get("trace-bin"), Some(&[0u8, 159, 255][..]));
/// assert!(metadata.add("grpc-timeout", "1S").is_err());
/// # Ok::<(), giong::error::Error>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Metadata {
    fields: Vec<(Box<str>, Box<[u8]>)>,
}

impl Metadata {
    /// No values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a value. Text values lose the spaces at their ends.
    ///
    /// # Errors
    ///
    /// `GIONG-E4101`
    /// ([`Code::GrpcInvalidMetadata`](giong::error::Code::GrpcInvalidMetadata))
    /// if the name is empty or has a character other than lowercase
    /// letters, digits, `-`, `_` and `.`, if it is one gRPC or HTTP uses
    /// itself (`grpc-...`, `content-type`, `te`, `host` and the like), or
    /// if the name does not end in `-bin` and the value has a byte outside
    /// printable ASCII. The message names the field without repeating the
    /// value.
    pub fn add(&mut self, name: &str, value: impl AsRef<[u8]>) -> Result<(), Error> {
        let mut value = value.as_ref();
        if !valid_name(name.as_bytes()) {
            return Err(invalid(
                format!("{name:?} is not a valid metadata name"),
                "use lowercase letters, digits, `-`, `_` and `.` only.",
            ));
        }
        if reserved(name) {
            return Err(invalid(
                format!("the metadata name {name} is used by gRPC or HTTP itself"),
                "choose another name: names that start with `grpc-` and the fields that \
                 frame the HTTP message or belong to one connection are set by \
                 giong-grpc.",
            ));
        }
        if !is_binary(name) {
            while let [b' ', rest @ ..] = value {
                value = rest;
            }
            while let [rest @ .., b' '] = value {
                value = rest;
            }
            if let Some(at) = value.iter().position(|b| !(0x20..=0x7e).contains(b)) {
                return Err(invalid(
                    format!(
                        "the value for the metadata name {name} has the byte {:#04x} at \
                         position {at}, outside printable ASCII",
                        value[at]
                    ),
                    "send bytes or text beyond ASCII under a name that ends in `-bin`.",
                ));
            }
        }
        self.fields.push((name.into(), value.into()));
        Ok(())
    }

    /// The first value named `name`.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|(n, _)| **n == *name)
            .map(|(_, v)| &**v)
    }

    /// [`get`](Self::get), if the value is UTF-8 (text values always are).
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| std::str::from_utf8(v).ok())
    }

    /// Every value named `name`, in order.
    pub fn get_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a [u8]> + 'a {
        self.fields
            .iter()
            .filter(move |(n, _)| **n == *name)
            .map(|(_, v)| &**v)
    }

    /// Every name and value, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.fields.iter().map(|(n, v)| (&**n, &**v))
    }

    /// The number of values.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// `true` without values.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Appends the values of `other`.
    pub(crate) fn extend(&mut self, other: &Metadata) {
        self.fields.extend(other.fields.iter().cloned());
    }

    /// Keeps a field received from the other end, if it is metadata: not a
    /// field of the call's machinery. `Err` with the name for a `-bin`
    /// value that is not base64.
    pub(crate) fn received(&mut self, name: &str, value: &[u8]) -> Result<(), String> {
        if !valid_name(name.as_bytes()) || reserved(name) {
            return Ok(());
        }
        if is_binary(name) {
            // Several values may come joined with commas.
            for part in value.split(|&b| b == b',') {
                let decoded = base64_decode(part).ok_or_else(|| name.to_owned())?;
                self.fields.push((name.into(), decoded.into()));
            }
        } else {
            self.fields.push((name.into(), value.into()));
        }
        Ok(())
    }

    /// The fields as they are sent: text values as they are, `-bin` values
    /// in base64 without padding.
    pub(crate) fn wire(&self) -> impl Iterator<Item = (&str, WireValue<'_>)> {
        self.fields.iter().map(|(n, v)| {
            if is_binary(n) {
                (&**n, WireValue::Encoded(base64_encode(v)))
            } else {
                // Printable ASCII when it was added here; as giong-http read
                // it when it was received, and checked again where it is sent.
                (&**n, WireValue::Text(v))
            }
        })
    }
}

/// A metadata value as it goes on the wire.
pub(crate) enum WireValue<'a> {
    Text(&'a [u8]),
    Encoded(String),
}

impl WireValue<'_> {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        match self {
            WireValue::Text(v) => v,
            WireValue::Encoded(v) => v.as_bytes(),
        }
    }
}

impl fmt::Debug for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.fields.iter().map(|(n, v)| {
                if is_binary(n) {
                    (n, format!("{} bytes", v.len()))
                } else {
                    (n, String::from_utf8_lossy(v).into_owned())
                }
            }))
            .finish()
    }
}

fn invalid(what: String, fix: &'static str) -> Error {
    Error::new(ErrorCode::GrpcInvalidMetadata, what, fix)
}

fn valid_name(name: &[u8]) -> bool {
    !name.is_empty()
        && name.iter().all(|&b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

fn reserved(name: &str) -> bool {
    name.starts_with("grpc-") || RESERVED.contains(&name)
}

fn is_binary(name: &str) -> bool {
    name.ends_with("-bin")
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 without padding, as gRPC sends `-bin` values.
pub(crate) fn base64_encode(data: &[u8]) -> String {
    let mut out = Vec::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = match *chunk {
            [a, b, c] => u32::from(a) << 16 | u32::from(b) << 8 | u32::from(c),
            [a, b] => u32::from(a) << 16 | u32::from(b) << 8,
            [a] => u32::from(a) << 16,
            _ => 0,
        };
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]);
        }
    }
    // The alphabet is ASCII.
    String::from_utf8(out).unwrap_or_default()
}

/// Decodes standard base64, with or without padding (senders differ).
/// `None` for a character outside the alphabet, padding in the middle, or
/// a length no encoder produces.
pub(crate) fn base64_decode(text: &[u8]) -> Option<Vec<u8>> {
    let mut text = text;
    let mut padding = 0;
    while let [rest @ .., b'='] = text {
        text = rest;
        padding += 1;
    }
    if padding > 2 || text.len() % 4 == 1 || (padding > 0 && (text.len() + padding) % 4 != 0) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3 + 2);
    for chunk in text.chunks(4) {
        let mut n = 0u32;
        for &c in chunk {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return None,
            };
            n = n << 6 | u32::from(v);
        }
        n <<= 6 * (4 - chunk.len());
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&bytes[..chunk.len() - 1]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_and_reads_both_paddings() {
        let vectors = [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ];
        for (plain, encoded) in vectors {
            assert_eq!(base64_encode(plain.as_bytes()), encoded);
            assert_eq!(base64_decode(encoded.as_bytes()).unwrap(), plain.as_bytes());
            let padded = format!("{encoded}{}", "=".repeat((4 - encoded.len() % 4) % 4));
            assert_eq!(base64_decode(padded.as_bytes()).unwrap(), plain.as_bytes());
        }
        for bad in ["Z", "Zg=", "Zg===", "Z=g", "Zm9v!", "Zm 9v", "Zm9vY"] {
            assert!(base64_decode(bad.as_bytes()).is_none(), "{bad}");
        }
        // Every byte value, every length around the chunk size.
        let all: Vec<u8> = (0..=255).collect();
        for len in 0..all.len() {
            let encoded = base64_encode(&all[..len]);
            assert_eq!(base64_decode(encoded.as_bytes()).unwrap(), &all[..len]);
        }
    }

    #[test]
    fn names_and_values_are_checked() {
        let mut m = Metadata::new();
        m.add("x-a", "  one ").unwrap();
        m.add("x-a", "two").unwrap();
        m.add("x-b-bin", [0u8, 255]).unwrap();
        assert_eq!(
            m.get_all("x-a").collect::<Vec<_>>(),
            [b"one" as &[u8], b"two"]
        );
        assert_eq!(m.len(), 3);
        for (name, value) in [
            ("", "v"),
            ("X-Upper", "v"),
            ("x a", "v"),
            ("grpc-status", "0"),
            ("content-type", "text/plain"),
            ("te", "trailers"),
            ("keep-alive", "timeout=5"),
            ("proxy-connection", "close"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "h2c"),
            ("x-a", "line\nbreak"),
            ("x-a", "caf\u{e9}"),
        ] {
            let error = m.add(name, value).unwrap_err();
            assert_eq!(error.code(), ErrorCode::GrpcInvalidMetadata, "{name}");
            assert!(error.fix().ends_with('.'));
            assert!(!error.to_string().contains("break"), "{error}");
        }
        assert_eq!(m.len(), 3);
        let wire: Vec<_> = m
            .wire()
            .map(|(n, v)| (n.to_owned(), v.as_bytes().to_vec()))
            .collect();
        assert_eq!(wire[2], ("x-b-bin".to_owned(), b"AP8".to_vec()));
    }

    #[test]
    fn received_fields_keep_metadata_only() {
        let mut m = Metadata::new();
        m.received("content-type", b"application/grpc").unwrap();
        m.received("grpc-timeout", b"1S").unwrap();
        m.received(":path", b"/a/b").unwrap();
        m.received("user-agent", b"grpc-go/1.70").unwrap();
        m.received("x-bin", b"AP8,Zg==").unwrap();
        assert_eq!(m.received("y-bin", b"!!"), Err("y-bin".to_owned()));
        assert_eq!(m.len(), 3);
        assert_eq!(m.get_str("user-agent"), Some("grpc-go/1.70"));
        assert_eq!(
            m.get_all("x-bin").collect::<Vec<_>>(),
            [&[0u8, 255][..], b"f"]
        );
    }
}
