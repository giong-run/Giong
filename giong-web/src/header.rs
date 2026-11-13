//! Checks on header names and values, the same giong-http applies.

/// A header name: an RFC 9110 token.
pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// A header value: visible characters, spaces and tabs, no CR or LF.
pub(crate) fn is_value(s: &str) -> bool {
    s.bytes().all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f))
}

/// `true` if the comma-separated list in `value` has `item`
/// (case-insensitive), as in `Vary: Origin, Accept-Encoding`.
pub(crate) fn list_contains(value: &str, item: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(item))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_values_and_lists() {
        assert!(is_token("X-Request-Id"));
        assert!(!is_token("Bad Name") && !is_token(""));
        assert!(is_value("a, b;q=0.5\tc") && is_value("é"));
        assert!(!is_value("a\r\nb"));
        assert!(list_contains("Origin, accept-encoding", "Accept-Encoding"));
        assert!(!list_contains("Origin", "Accept-Encoding"));
    }
}
