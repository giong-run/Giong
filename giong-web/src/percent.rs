//! Percent-decoding of path segments and query strings.

use std::borrow::Cow;

/// Why a percent-encoded string could not be decoded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// A `%` not followed by two hexadecimal digits.
    BadEscape,
    /// The decoded bytes are not UTF-8.
    NotUtf8,
}

impl DecodeError {
    pub(crate) fn describe(&self) -> &'static str {
        match self {
            DecodeError::BadEscape => "a `%` is not followed by two hexadecimal digits",
            DecodeError::NotUtf8 => "the percent-decoded bytes are not UTF-8",
        }
    }
}

/// Decodes `%XX` escapes. A path segment: `+` stays a `+`.
pub(crate) fn decode(s: &str) -> Result<Cow<'_, str>, DecodeError> {
    decode_inner(s, false)
}

/// Decodes a query-string key or value: `%XX` escapes, and `+` as a space.
pub(crate) fn decode_query(s: &str) -> Result<Cow<'_, str>, DecodeError> {
    decode_inner(s, true)
}

fn decode_inner(s: &str, plus_is_space: bool) -> Result<Cow<'_, str>, DecodeError> {
    let bytes = s.as_bytes();
    if !bytes
        .iter()
        .any(|&b| b == b'%' || (plus_is_space && b == b'+'))
    {
        return Ok(Cow::Borrowed(s));
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = bytes.get(i + 1).and_then(|&b| hex(b));
                let lo = bytes.get(i + 2).and_then(|&b| hex(b));
                match (hi, lo) {
                    (Some(hi), Some(lo)) => out.push(hi << 4 | lo),
                    _ => return Err(DecodeError::BadEscape),
                }
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out)
        .map(Cow::Owned)
        .map_err(|_| DecodeError::NotUtf8)
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The `key=value` pairs of a query string, still encoded. Empty pairs
/// (`a=1&&b=2`) are skipped; a key without `=` has an empty value.
pub(crate) fn query_pairs(query: &str) -> impl Iterator<Item = (&str, &str)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_escapes_and_leaves_plain_text_borrowed() {
        assert!(matches!(decode("plain"), Ok(Cow::Borrowed("plain"))));
        assert_eq!(decode("caf%C3%A9%20au+lait").unwrap(), "café au+lait");
        assert_eq!(decode_query("caf%c3%a9+au+lait").unwrap(), "café au lait");
        assert_eq!(decode("a%2Fb").unwrap(), "a/b");
        assert_eq!(decode("100%"), Err(DecodeError::BadEscape));
        assert_eq!(decode("%zz"), Err(DecodeError::BadEscape));
        assert_eq!(decode("%ff"), Err(DecodeError::NotUtf8));
    }

    #[test]
    fn splits_a_query_into_pairs() {
        let pairs: Vec<_> = query_pairs("a=1&&b=&c&d=x=y").collect();
        assert_eq!(pairs, [("a", "1"), ("b", ""), ("c", ""), ("d", "x=y")]);
        assert_eq!(query_pairs("").count(), 0);
    }
}
