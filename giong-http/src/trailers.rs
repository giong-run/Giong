//! Trailer fields: the fields sent after a body, in a last HEADERS frame
//! over HTTP/2 or after the last chunk of a chunked HTTP/1.1 body.

use std::fmt;

use giong::error::{Code, Error};

use crate::parse::{is_field_value, is_token_str, trim};

/// Fields that describe the message's framing, routing or credentials, or
/// one connection: they mean nothing after the body (RFC 9110, section
/// 6.5.1), so they are refused when trailers are built and dropped when
/// trailers are received.
const NOT_IN_TRAILERS: [&str; 20] = [
    "authorization",
    "cache-control",
    "connection",
    "content-encoding",
    "content-length",
    "content-range",
    "content-type",
    "cookie",
    "expect",
    "host",
    "keep-alive",
    "max-forwards",
    "proxy-authorization",
    "proxy-connection",
    "range",
    "set-cookie",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// The fields sent after a body: what a streamed response ends with
/// ([`BodySender::send_trailers`](crate::BodySender::send_trailers)), and
/// what a streamed request body ended with
/// ([`BodyStream::trailers`](crate::BodyStream::trailers)).
///
/// Names are kept in lowercase and in the order they were added; a name
/// may appear more than once. Values are bytes, as in
/// [`Request::header`](crate::Request::header).
///
/// ```
/// use giong_http::Trailers;
///
/// let trailers = Trailers::new()
///     .field("Grpc-Status", "0")?
///     .field("grpc-message", "")?;
/// assert_eq!(trailers.get_str("grpc-status"), Some("0"));
/// assert_eq!(trailers.len(), 2);
/// assert!(Trailers::new().field("content-length", "3").is_err());
/// # Ok::<(), giong::error::Error>(())
/// ```
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Trailers {
    fields: Vec<(Box<str>, Box<[u8]>)>,
}

impl Trailers {
    /// No fields.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a field. The name is stored in lowercase and the value without
    /// spaces or tabs at its ends.
    ///
    /// # Errors
    ///
    /// `GIONG-E1122`
    /// ([`Code::HttpInvalidTrailer`](giong::error::Code::HttpInvalidTrailer))
    /// if `name` is not a token, if it is a field that cannot be sent after
    /// a body (`content-length`, `transfer-encoding`, `host`, `connection`,
    /// `content-type`, `authorization`, `set-cookie` and the like), or if
    /// `value` has a control character other than a tab (CR and LF
    /// included). The message names the field without repeating the value.
    pub fn field(mut self, name: &str, value: impl AsRef<[u8]>) -> Result<Self, Error> {
        let value = trim(value.as_ref());
        if !is_token_str(name.as_bytes()) {
            return Err(invalid(
                format!("{name:?} is not a valid trailer field name"),
                "a field name is a token: letters, digits and !#$%&'*+-.^_`|~, without \
                 spaces or a colon.",
            ));
        }
        let name = name.to_ascii_lowercase();
        if NOT_IN_TRAILERS.contains(&name.as_str()) {
            return Err(invalid(
                format!("the field {name} cannot be sent in trailers"),
                "send it as a header of the message instead: fields about the framing, \
                 routing, credentials or connection of a message mean nothing after its \
                 body.",
            ));
        }
        if let Some(at) = value
            .iter()
            .position(|&b| b != b'\t' && (b < 0x20 || b == 0x7f))
        {
            return Err(invalid(
                format!(
                    "the value for the trailer field {name} has the control character \
                     {:#04x} at byte {at}",
                    value[at]
                ),
                "encode the value first (percent-encoding or base64): a line break in a \
                 value would start a field of its own.",
            ));
        }
        self.push(name.into_boxed_str(), value.into());
        Ok(self)
    }

    /// The value of the first field named `name`, compared without regard
    /// to case.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| &**v)
    }

    /// [`get`](Self::get), if the value is UTF-8.
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|v| std::str::from_utf8(v).ok())
    }

    /// Every field, in order: the name in lowercase, and the value.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.fields.iter().map(|(n, v)| (&**n, &**v))
    }

    /// The number of fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// `true` without fields.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    fn push(&mut self, name: Box<str>, value: Box<[u8]>) {
        self.fields.push((name, value));
    }

    /// Keeps a field received from the other end. `false` if the name or
    /// the value is not valid; a valid field that means nothing in trailers
    /// is dropped.
    pub(crate) fn received(&mut self, name: &[u8], value: &[u8]) -> bool {
        let value = trim(value);
        if !is_token_str(name) || !is_field_value(value) {
            return false;
        }
        let Ok(name) = std::str::from_utf8(name) else {
            return false;
        };
        let name = name.to_ascii_lowercase();
        if !NOT_IN_TRAILERS.contains(&name.as_str()) {
            self.push(name.into_boxed_str(), value.into());
        }
        true
    }

    /// The fields as HTTP/1.1 lines, each ended by CRLF.
    pub(crate) fn write_lines(&self, out: &mut Vec<u8>) {
        for (name, value) in &self.fields {
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(b": ");
            out.extend_from_slice(value);
            out.extend_from_slice(b"\r\n");
        }
    }
}

impl fmt::Debug for Trailers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                self.fields
                    .iter()
                    .map(|(n, v)| (n, String::from_utf8_lossy(v))),
            )
            .finish()
    }
}

fn invalid(what: String, fix: &'static str) -> Error {
    Error::new(Code::HttpInvalidTrailer, what, fix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_are_checked_and_kept_in_order() {
        let t = Trailers::new()
            .field("X-Sum", " abc\t")
            .unwrap()
            .field("x-sum", "def")
            .unwrap()
            .field("x-bin", [0xffu8, 0x80])
            .unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t.get("X-SUM"), Some(&b"abc"[..]));
        assert_eq!(t.get_str("x-bin"), None);
        assert_eq!(
            t.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            ["x-sum", "x-sum", "x-bin"]
        );
        let mut lines = Vec::new();
        t.write_lines(&mut lines);
        assert!(lines.starts_with(b"x-sum: abc\r\nx-sum: def\r\nx-bin: "));
    }

    #[test]
    fn invalid_fields_are_refused_with_the_code() {
        for (name, value) in [
            ("", "v"),
            ("a b", "v"),
            ("a:", "v"),
            ("Content-Length", "3"),
            ("transfer-encoding", "chunked"),
            ("host", "example.com"),
            ("x-a", "line\r\nbreak: 1"),
            ("x-a", "nul\0"),
        ] {
            let error = Trailers::new().field(name, value).unwrap_err();
            assert_eq!(error.code(), Code::HttpInvalidTrailer, "{name}");
            assert!(error.fix().ends_with('.'), "{error}");
            assert!(!error.to_string().contains("break: 1"), "{error}");
        }
    }

    #[test]
    fn received_fields_that_mean_nothing_are_dropped() {
        let mut t = Trailers::new();
        assert!(t.received(b"Content-Length", b"3"));
        assert!(t.received(b"X-Ok", b" yes "));
        assert!(!t.received(b"bad name", b"v"));
        assert!(!t.received(b"x-a", b"a\nb"));
        assert_eq!(t.len(), 1);
        assert_eq!(t.get_str("x-ok"), Some("yes"));
    }
}
