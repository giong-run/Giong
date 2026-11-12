//! Response head parsing, and how the body that follows is framed
//! (RFC 9112 section 6.3).
//!
//! Strict where a mistake would let one response be read as part of
//! another on a kept-alive connection: two different lengths, bare line
//! feeds, folded or malformed header lines.

use std::ops::Range;

use crate::parse::{is_field_value, is_token_str, trim};

/// A parsed response head. Ranges are relative to the start of the head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) status: u16,
    /// The `x` in `HTTP/1.x`.
    pub(crate) minor: u8,
    /// Every header line, as (name, value).
    pub(crate) fields: Vec<(Range<usize>, Range<usize>)>,
    /// Length of the whole head, including the empty line that ends it.
    pub(crate) len: usize,
    pub(crate) content_length: Option<u64>,
    pub(crate) chunked: bool,
    /// The connection cannot carry another request after this response.
    pub(crate) close: bool,
}

/// How the body after a head is delimited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    /// No body (a response to `HEAD`, 204, 304).
    None,
    Length(u64),
    Chunked,
    /// Until the server closes the connection.
    UntilClose,
}

/// Parses a complete head (`head` ends with the empty line). On failure,
/// says what was wrong.
pub(crate) fn parse_head(head: &[u8]) -> Result<Head, String> {
    debug_assert!(head.ends_with(b"\r\n\r\n"));
    let mut lines = Lines { head, pos: 0 };
    let (_, line) = lines.next_line()?.ok_or("the response is empty")?;

    // status-line = HTTP-version SP status-code SP [ reason-phrase ]
    let minor = match line.get(..9) {
        Some(b"HTTP/1.1 ") => 1,
        Some(b"HTTP/1.0 ") => 0,
        _ => {
            let shown = String::from_utf8_lossy(&line[..line.len().min(20)]).into_owned();
            return Err(format!(
                "the response starts with {shown:?}, not HTTP/1.1 or HTTP/1.0 and a status"
            ));
        }
    };
    let status = match line.get(9..12) {
        Some(digits) if digits.iter().all(u8::is_ascii_digit) => digits
            .iter()
            .fold(0u16, |n, &d| n * 10 + u16::from(d - b'0')),
        _ => return Err("the status line has no three-digit status".into()),
    };
    if !(100..=599).contains(&status) {
        return Err(format!("the status {status} is outside 100 to 599"));
    }
    // Some servers leave out the space before an empty reason.
    match line.get(12) {
        None => {}
        Some(b' ') if is_field_value(&line[13..]) => {}
        _ => return Err("the status line has something malformed after the status".into()),
    }

    let mut parsed = Head {
        status,
        minor,
        fields: Vec::new(),
        len: head.len(),
        content_length: None,
        chunked: false,
        close: minor == 0,
    };
    let mut keep_alive = false;
    let mut closing = false;
    let mut transfer_encoding = false;
    while let Some((start, line)) = lines.next_line()? {
        let colon = line
            .iter()
            .position(|&b| b == b':')
            .ok_or("a header line has no colon")?;
        let (name, raw_value) = (&line[..colon], &line[colon + 1..]);
        if !is_token_str(name) {
            return Err(format!(
                "the header name {:?} is not valid (whitespace before the colon, or a folded line?)",
                String::from_utf8_lossy(name)
            ));
        }
        let value = trim(raw_value);
        if !is_field_value(value) {
            return Err(format!(
                "the {} header has a control character in its value",
                String::from_utf8_lossy(name)
            ));
        }
        let lead = raw_value
            .iter()
            .take_while(|&&b| b == b' ' || b == b'\t')
            .count();
        let value_start = start + colon + 1 + lead;
        parsed
            .fields
            .push((start..start + colon, value_start..value_start + value.len()));

        if name.eq_ignore_ascii_case(b"content-length") {
            if value.is_empty() || value.len() > 19 || !value.iter().all(u8::is_ascii_digit) {
                return Err(format!(
                    "Content-Length is {:?}, not a number",
                    String::from_utf8_lossy(value)
                ));
            }
            let n = value
                .iter()
                .fold(0u64, |acc, &b| acc * 10 + u64::from(b - b'0'));
            if parsed.content_length.is_some_and(|prev| prev != n) {
                return Err("the response has two different Content-Length headers".into());
            }
            parsed.content_length = Some(n);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if transfer_encoding {
                return Err("the response has two Transfer-Encoding headers".into());
            }
            transfer_encoding = true;
            if !value.eq_ignore_ascii_case(b"chunked") {
                return Err(format!(
                    "the response uses Transfer-Encoding: {}, which this client does not decode \
                     (only chunked)",
                    String::from_utf8_lossy(value)
                ));
            }
            parsed.chunked = true;
        } else if name.eq_ignore_ascii_case(b"connection") {
            for token in value.split(|&b| b == b',').map(trim) {
                if token.eq_ignore_ascii_case(b"close") {
                    closing = true;
                    parsed.close = true;
                } else if token.eq_ignore_ascii_case(b"keep-alive") {
                    keep_alive = true;
                }
            }
        }
    }
    // HTTP/1.0 closes after each response unless the server says otherwise.
    if minor == 0 && keep_alive && !closing {
        parsed.close = false;
    }
    if parsed.chunked && minor == 0 {
        return Err("an HTTP/1.0 response cannot use Transfer-Encoding".into());
    }
    if parsed.chunked && parsed.content_length.is_some() {
        // RFC 9112 section 6.3: the chunked encoding wins, and the
        // connection must not be used again.
        parsed.content_length = None;
        parsed.close = true;
    }
    Ok(parsed)
}

impl Head {
    /// How the body is delimited, for a request with this method.
    pub(crate) fn framing(&self, head_request: bool) -> Framing {
        if head_request || self.status == 204 || self.status == 304 || self.status < 200 {
            Framing::None
        } else if self.chunked {
            Framing::Chunked
        } else if let Some(n) = self.content_length {
            Framing::Length(n)
        } else {
            Framing::UntilClose
        }
    }
}

/// The CRLF-terminated lines of a head, up to the empty one.
struct Lines<'a> {
    head: &'a [u8],
    pos: usize,
}

impl<'a> Lines<'a> {
    /// The next line and where it starts in the head; `None` at the empty
    /// line.
    fn next_line(&mut self) -> Result<Option<(usize, &'a [u8])>, String> {
        let rest = &self.head[self.pos..];
        let nl = rest
            .iter()
            .position(|&b| b == b'\n')
            .ok_or("the head is not ended by an empty line")?;
        if nl == 0 || rest[nl - 1] != b'\r' {
            return Err("a line of the head ends in a bare line feed".into());
        }
        let line = &rest[..nl - 1];
        let start = self.pos;
        self.pos += nl + 1;
        if line.contains(&b'\r') {
            return Err("a line of the head has a carriage return in it".into());
        }
        Ok((!line.is_empty()).then_some((start, line)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Head, String> {
        parse_head(s.as_bytes())
    }

    #[test]
    fn a_typical_response_is_read() {
        let s = "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length:  12 \r\nX-Empty:\r\n\r\n";
        let h = parse(s).unwrap();
        assert_eq!(
            (h.status, h.minor, h.len, h.close),
            (200, 1, s.len(), false)
        );
        assert_eq!(h.content_length, Some(12));
        assert_eq!(h.framing(false), Framing::Length(12));
        assert_eq!(h.framing(true), Framing::None);
        let fields: Vec<(&str, &str)> = h
            .fields
            .iter()
            .map(|(n, v)| (&s[n.clone()], &s[v.clone()]))
            .collect();
        assert_eq!(
            fields,
            [
                ("Content-Type", "text/plain"),
                ("Content-Length", "12"),
                ("X-Empty", "")
            ]
        );
    }

    #[test]
    fn framing_follows_rfc_9112() {
        let h = parse("HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n").unwrap();
        assert_eq!(h.framing(false), Framing::Chunked);
        let h = parse("HTTP/1.1 200 OK\r\n\r\n").unwrap();
        assert_eq!(h.framing(false), Framing::UntilClose);
        for status in [100, 103, 204, 304] {
            let h = parse(&format!("HTTP/1.1 {status} X\r\nContent-Length: 5\r\n\r\n")).unwrap();
            assert_eq!(h.framing(false), Framing::None, "{status}");
        }
        // Both: chunked wins and the connection is not used again.
        let h = parse("HTTP/1.1 200 OK\r\nContent-Length: 5\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        assert_eq!((h.framing(false), h.close), (Framing::Chunked, true));
        // The same length twice is fine.
        assert!(parse("HTTP/1.1 200 OK\r\nContent-Length: 5\r\ncontent-length: 5\r\n\r\n").is_ok());
    }

    #[test]
    fn keep_alive_follows_the_version_and_connection() {
        assert!(!parse("HTTP/1.1 200 OK\r\n\r\n").unwrap().close);
        assert!(
            parse("HTTP/1.1 200 OK\r\nConnection: Upgrade, close\r\n\r\n")
                .unwrap()
                .close
        );
        assert!(parse("HTTP/1.0 200 OK\r\n\r\n").unwrap().close);
        assert!(
            !parse("HTTP/1.0 200 OK\r\nConnection: keep-alive\r\n\r\n")
                .unwrap()
                .close
        );
        assert!(
            parse("HTTP/1.0 200 OK\r\nConnection: keep-alive, close\r\n\r\n")
                .unwrap()
                .close
        );
    }

    #[test]
    fn status_lines_without_a_reason_are_read() {
        assert_eq!(parse("HTTP/1.1 404\r\n\r\n").unwrap().status, 404);
        assert_eq!(parse("HTTP/1.1 404 \r\n\r\n").unwrap().status, 404);
        assert_eq!(
            parse("HTTP/1.1 500 Internal Server Error\r\n\r\n")
                .unwrap()
                .status,
            500
        );
    }

    #[test]
    fn malformed_or_ambiguous_heads_are_refused() {
        for (input, says) in [
            ("HTTP/2 200 OK\r\n\r\n", "not HTTP/1.1"),
            ("ICY 200 OK\r\n\r\n", "not HTTP/1.1"),
            ("HTTP/1.1 2000 OK\r\n\r\n", "malformed after the status"),
            ("HTTP/1.1 20 OK\r\n\r\n", "three-digit"),
            ("HTTP/1.1 099 OK\r\n\r\n", "outside 100 to 599"),
            ("HTTP/1.1 600 OK\r\n\r\n", "outside 100 to 599"),
            ("HTTP/1.1 200 O\x01K\r\n\r\n", "malformed after the status"),
            ("HTTP/1.1 200 OK\nServer: x\r\n\r\n", "bare line feed"),
            ("HTTP/1.1 200 OK\r\nA: b\rc\r\n\r\n", "carriage return"),
            ("HTTP/1.1 200 OK\r\nNoColon\r\n\r\n", "no colon"),
            ("HTTP/1.1 200 OK\r\nBad Name: 1\r\n\r\n", "not valid"),
            ("HTTP/1.1 200 OK\r\nName : 1\r\n\r\n", "not valid"),
            ("HTTP/1.1 200 OK\r\nA: 1\r\n folded\r\n\r\n", "no colon"),
            ("HTTP/1.1 200 OK\r\nA: b\x00c\r\n\r\n", "control character"),
            ("HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n", "two different"),
            ("HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n", "not a number"),
            ("HTTP/1.1 200 OK\r\nContent-Length: 1, 1\r\n\r\n", "not a number"),
            ("HTTP/1.1 200 OK\r\nContent-Length: 99999999999999999999\r\n\r\n", "not a number"),
            ("HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n", "does not decode"),
            ("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n", "two Transfer-Encoding"),
            ("HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n", "HTTP/1.0"),
        ] {
            let e = parse(input).unwrap_err();
            assert!(e.contains(says), "{input:?}: {e}");
        }
    }
}
