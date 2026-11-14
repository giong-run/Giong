//! Request head parsing. Strict by design: anything ambiguous is rejected,
//! because ambiguity between a proxy and this server is how request smuggling
//! works.

use std::ops::Range;

/// A parsed request head. All ranges are relative to the start of the head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Head {
    pub(crate) method: Range<usize>,
    pub(crate) target: Range<usize>,
    /// The `x` in `HTTP/1.x`.
    pub(crate) minor: u8,
    /// The header lines, without the request line and the final empty line.
    pub(crate) headers: Range<usize>,
    /// Length of the whole head, including the terminating empty line.
    pub(crate) len: usize,
    pub(crate) content_length: Option<u64>,
    pub(crate) chunked: bool,
    /// `Connection: close` was sent.
    pub(crate) close: bool,
    /// `Connection: keep-alive` was sent.
    pub(crate) keep_alive: bool,
    pub(crate) expect_continue: bool,
    /// The request came over HTTP/2 (the head was rebuilt from its
    /// header fields; see `h2::request`).
    pub(crate) h2: bool,
}

/// Position just past the `\r\n\r\n` that ends a head, searching from `from`.
pub(crate) fn find_head_end(buf: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while let Some(off) = buf.get(i..)?.iter().position(|&b| b == b'\n') {
        let nl = i + off;
        if nl >= 3 && &buf[nl - 3..nl] == b"\r\n\r" {
            return Some(nl + 1);
        }
        i = nl + 1;
    }
    None
}

fn is_token(b: u8) -> bool {
    matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' |
        b'^' | b'_' | b'`' | b'|' | b'~' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}

pub(crate) fn is_token_str(s: &[u8]) -> bool {
    !s.is_empty() && s.iter().all(|&b| is_token(b))
}

/// Header values: visible ASCII, space, tab and obs-text. No other controls.
pub(crate) fn is_field_value(s: &[u8]) -> bool {
    s.iter().all(|&b| b == b'\t' || (b >= 0x20 && b != 0x7f))
}

pub(crate) fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// Splits a request target in absolute form, `scheme://authority[rest]`,
/// into its scheme, its authority (up to the first `/`, `?` or `#`) and the
/// rest, which may be empty. `None` if the target does not start with a
/// scheme (RFC 3986, section 3.1) and `://`.
pub(crate) fn split_absolute(target: &[u8]) -> Option<(&[u8], &[u8], &[u8])> {
    let colon = target.iter().position(|&b| b == b':')?;
    let scheme = &target[..colon];
    if !scheme.first()?.is_ascii_alphabetic()
        || !scheme
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return None;
    }
    let rest = target[colon + 1..].strip_prefix(b"//")?;
    let end = rest
        .iter()
        .position(|&b| matches!(b, b'/' | b'?' | b'#'))
        .unwrap_or(rest.len());
    Some((scheme, &rest[..end], &rest[end..]))
}

/// Whether a `Host` header and the authority of a request (an absolute
/// target's over HTTP/1.1, `:authority` over HTTP/2) name the same host.
/// Host names are case-insensitive, and nothing else is normalised: a port
/// given on one side only, even the default one, is a difference.
pub(crate) fn same_authority(host: &[u8], authority: &[u8]) -> bool {
    host.eq_ignore_ascii_case(authority)
}

/// Splits a block of header lines into `(name, value)` pairs. The block must
/// have passed [`parse_head`].
pub(crate) fn header_lines(block: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    block
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let colon = line.iter().position(|&b| b == b':')?;
            Some((&line[..colon], trim(&line[colon + 1..])))
        })
}

/// Parses a complete head (`head` ends with the empty line). On failure returns
/// the status code to answer with.
pub(crate) fn parse_head(head: &[u8]) -> Result<Head, u16> {
    debug_assert!(head.ends_with(b"\r\n\r\n"));
    let line_end = head
        .iter()
        .position(|&b| b == b'\n')
        .filter(|&i| i > 0 && head[i - 1] == b'\r')
        .ok_or(400u16)?;
    let line = &head[..line_end - 1];

    // request-line = method SP request-target SP HTTP-version
    let sp1 = line.iter().position(|&b| b == b' ').ok_or(400u16)?;
    let sp2 = sp1
        + 1
        + line[sp1 + 1..]
            .iter()
            .position(|&b| b == b' ')
            .ok_or(400u16)?;
    let (method, target, version) = (&line[..sp1], &line[sp1 + 1..sp2], &line[sp2 + 1..]);
    if !is_token_str(method) || method.len() > 32 {
        return Err(400);
    }
    // Visible ASCII only, so the target is always valid UTF-8.
    if target.is_empty() || !target.iter().all(|&b| (0x21..=0x7e).contains(&b)) {
        return Err(400);
    }
    // The version first: the rules below are HTTP/1.x's, and another
    // version (`PRI * HTTP/2.0`, the HTTP/2 preface) is answered with 505
    // whatever its target.
    let minor = match version {
        b"HTTP/1.1" => 1,
        b"HTTP/1.0" => 0,
        [b'H', b'T', b'T', b'P', b'/', major, b'.', minor]
            if major.is_ascii_digit() && minor.is_ascii_digit() =>
        {
            return Err(505)
        }
        _ => return Err(400),
    };
    // The target is a path (origin form), `*` for OPTIONS, `host:port` for
    // CONNECT, or a whole URL (absolute form, RFC 9112 section 3.2.2), as
    // a client sends to a proxy. A server must take the host from such a
    // URL and ignore `Host`, but handlers read `Host`: a proxy in front
    // that routes on the URL while the handler picks the site from `Host`
    // could be made to disagree. So the URL must be `http` or `https` and
    // name the same host as `Host`, as `:authority` must over HTTP/2.
    let authority = match target {
        [b'/', ..] => None,
        b"*" if method == b"OPTIONS" => None,
        _ if method == b"CONNECT" => None,
        _ => {
            let (scheme, authority, _) = split_absolute(target).ok_or(400u16)?;
            let http =
                scheme.eq_ignore_ascii_case(b"http") || scheme.eq_ignore_ascii_case(b"https");
            // No user name or password either (RFC 9110, section 4.2.4).
            if !http || authority.is_empty() || authority.contains(&b'@') {
                return Err(400);
            }
            Some(authority)
        }
    };

    let headers = line_end + 1..head.len() - 2;
    let mut parsed = Head {
        method: 0..sp1,
        target: sp1 + 1..sp2,
        minor,
        headers: headers.clone(),
        len: head.len(),
        content_length: None,
        chunked: false,
        close: false,
        keep_alive: false,
        expect_continue: false,
        h2: false,
    };
    let mut hosts = 0;
    let mut host: Option<&[u8]> = None;
    let mut transfer_encoding = false;

    let mut rest = &head[headers];
    while !rest.is_empty() {
        // Every line ends in CRLF; a bare LF or CR anywhere is an error.
        let nl = rest.iter().position(|&b| b == b'\n').ok_or(400u16)?;
        if nl == 0 || rest[nl - 1] != b'\r' {
            return Err(400);
        }
        let line = &rest[..nl - 1];
        rest = &rest[nl + 1..];

        // No obsolete line folding, no whitespace around the name.
        let colon = line.iter().position(|&b| b == b':').ok_or(400u16)?;
        let (name, value) = (&line[..colon], trim(&line[colon + 1..]));
        if !is_token_str(name) || !is_field_value(value) {
            return Err(400);
        }

        if name.eq_ignore_ascii_case(b"content-length") {
            if value.is_empty() || value.len() > 19 || !value.iter().all(u8::is_ascii_digit) {
                return Err(400);
            }
            let n = value
                .iter()
                .fold(0u64, |acc, &b| acc * 10 + (b - b'0') as u64);
            if parsed.content_length.is_some_and(|prev| prev != n) {
                return Err(400);
            }
            parsed.content_length = Some(n);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if transfer_encoding || minor == 0 {
                return Err(400);
            }
            if !value.eq_ignore_ascii_case(b"chunked") {
                return Err(501);
            }
            transfer_encoding = true;
            parsed.chunked = true;
        } else if name.eq_ignore_ascii_case(b"connection") {
            for token in value.split(|&b| b == b',').map(trim) {
                if token.eq_ignore_ascii_case(b"close") {
                    parsed.close = true;
                } else if token.eq_ignore_ascii_case(b"keep-alive") {
                    parsed.keep_alive = true;
                }
            }
        } else if name.eq_ignore_ascii_case(b"expect") {
            if !value.eq_ignore_ascii_case(b"100-continue") {
                return Err(417);
            }
            parsed.expect_continue = true;
        } else if name.eq_ignore_ascii_case(b"host") {
            hosts += 1;
            host = Some(value);
        }
    }

    // A message with both framings is the classic smuggling vector.
    if parsed.chunked && parsed.content_length.is_some() {
        return Err(400);
    }
    if hosts > 1 || (minor == 1 && hosts == 0) {
        return Err(400);
    }
    // An HTTP/1.0 request in absolute form without `Host` is refused too:
    // the handler would see no host where the proxy saw one.
    if let Some(authority) = authority {
        if !host.is_some_and(|host| same_authority(host, authority)) {
            return Err(400);
        }
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Head, u16> {
        parse_head(s.as_bytes())
    }

    #[test]
    fn finds_the_end_of_a_head_from_any_offset() {
        let buf = b"GET / HTTP/1.1\r\nHost: x\r\n\r\nrest";
        assert_eq!(find_head_end(buf, 0), Some(27));
        for from in 0..=26 {
            assert_eq!(find_head_end(buf, from.min(23)), Some(27), "from {from}");
        }
        assert_eq!(find_head_end(&buf[..26], 0), None);
        assert_eq!(find_head_end(b"\n\n\r\n", 0), None);
        assert_eq!(find_head_end(b"", 0), None);
        assert_eq!(find_head_end(b"abc", 7), None);
    }

    #[test]
    fn parses_a_typical_request() {
        let s = "POST /a/b?x=1 HTTP/1.1\r\nHost: example.com\r\nContent-Length: 12\r\nConnection: Keep-Alive, Upgrade\r\n\r\n";
        let h = parse(s).unwrap();
        assert_eq!(&s[h.method.clone()], "POST");
        assert_eq!(&s[h.target.clone()], "/a/b?x=1");
        assert_eq!(h.minor, 1);
        assert_eq!(h.len, s.len());
        assert_eq!(h.content_length, Some(12));
        assert!(h.keep_alive && !h.close && !h.chunked && !h.expect_continue);
        let lines: Vec<_> = header_lines(&s.as_bytes()[h.headers.clone()]).collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], (&b"Host"[..], &b"example.com"[..]));
    }

    #[test]
    fn recognises_framing_and_connection_headers() {
        let h = parse("PUT / HTTP/1.1\r\nhost: x\r\nTRANSFER-ENCODING: Chunked\r\nconnection: close\r\nExpect: 100-Continue\r\n\r\n").unwrap();
        assert!(h.chunked && h.close && h.expect_continue);
        assert_eq!(h.content_length, None);
        let h = parse("GET / HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(h.minor, 0);
        // Repeating the same length is allowed, a different one is not.
        assert!(parse(
            "GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 3\r\nContent-Length: 3\r\n\r\n"
        )
        .is_ok());
    }

    #[test]
    fn rejects_malformed_and_ambiguous_heads() {
        let cases: &[(&str, u16)] = &[
            ("GET /\r\n\r\n", 400),
            ("GET  / HTTP/1.1\r\nHost: x\r\n\r\n", 400),
            ("GET / HTTP/1.1 \r\nHost: x\r\n\r\n", 400),
            ("GET / HTTP/2.0\r\nHost: x\r\n\r\n", 505),
            ("PRI * HTTP/2.0\r\n\r\n", 505),
            ("GET / FTP/1.1\r\nHost: x\r\n\r\n", 400),
            ("G@T / HTTP/1.1\r\nHost: x\r\n\r\n", 400),
            ("GET /a\tb HTTP/1.1\r\nHost: x\r\n\r\n", 400),
            ("GET /\u{e9} HTTP/1.1\r\nHost: x\r\n\r\n", 400),
            // Host is mandatory in HTTP/1.1 and must be unique.
            ("GET / HTTP/1.1\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n", 400),
            // Header syntax.
            ("GET / HTTP/1.1\r\nHost: x\r\nNoColon\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nBad Name: 1\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nName : 1\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\n folded: 1\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\n: empty\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nA: b\0c\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\nA: b\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nA: b\rc\r\n\r\n", 400),
            // Body framing.
            ("GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1, 1\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nContent-Length: -1\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nContent-Length: +1\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nContent-Length: \r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 99999999999999999999\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nTransfer-Encoding: gzip, chunked\r\n\r\n", 501),
            ("GET / HTTP/1.0\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
            ("GET / HTTP/1.1\r\nHost: x\r\nExpect: something\r\n\r\n", 417),
        ];
        for (input, status) in cases {
            assert_eq!(parse(input), Err(*status), "{input:?}");
        }
    }

    #[test]
    fn an_absolute_target_must_name_the_host_of_the_host_header() {
        let accepted = [
            "GET http://example.com/a?b=1 HTTP/1.1\r\nHost: example.com\r\n\r\n",
            // Schemes and host names are case-insensitive.
            "GET HTTPS://Example.COM/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET Http://example.com/ HTTP/1.1\r\nHost: EXAMPLE.com\r\n\r\n",
            // A port is part of the authority, on both sides.
            "GET http://example.com:8080/a HTTP/1.1\r\nHost: example.com:8080\r\n\r\n",
            "GET http://[::1]:8080/a HTTP/1.1\r\nHost: [::1]:8080\r\n\r\n",
            // No path, or a query straight after the authority.
            "GET http://example.com HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET http://example.com?q=/x HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET http://example.com/ HTTP/1.0\r\nHost: example.com\r\n\r\n",
            // The other forms of target, for the methods that use them.
            "OPTIONS * HTTP/1.1\r\nHost: x\r\n\r\n",
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
        ];
        for input in accepted {
            assert!(parse(input).is_ok(), "{input:?} was refused");
        }
        let refused = [
            // RFC 9112, section 3.2.2: the target's authority is the host,
            // so a Host that names another one is a disagreement.
            "GET http://internal.example/admin HTTP/1.1\r\nHost: public.example\r\n\r\n",
            "GET http://example.com:8080/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET http://example.com/ HTTP/1.1\r\nHost: example.com:80\r\n\r\n",
            "GET http://example.com/ HTTP/1.0\r\n\r\n",
            // Only http and https.
            "GET foo://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET ftp://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET httpx://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET ws://example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
            // No user name or password (RFC 9110, section 4.2.4), even
            // when Host repeats them.
            "GET http://user@example.com/ HTTP/1.1\r\nHost: example.com\r\n\r\n",
            "GET http://u:p@example.com/ HTTP/1.1\r\nHost: u:p@example.com\r\n\r\n",
            // No authority, or not a target at all.
            "GET http:///a HTTP/1.1\r\nHost: \r\n\r\n",
            "GET http:/a HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET a/b HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET ?q HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n",
            "GET * HTTP/1.1\r\nHost: x\r\n\r\n",
        ];
        for input in refused {
            assert_eq!(parse(input), Err(400), "{input:?}");
        }
    }
}
