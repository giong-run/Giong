//! `Alt-Svc` (RFC 7838), as far as finding an origin's HTTP/3 goes.
//!
//! A server that also speaks HTTP/3 says so on its responses over TCP:
//! `alt-svc: h3=":443"; ma=86400`. The client follows an alternative on
//! the origin's own host only: one that names another host is ignored, so
//! that a response can never send later requests to another machine.

use std::time::Duration;

/// How long an alternative is used when the header does not say (`ma`,
/// section 3.1).
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest an alternative is remembered, whatever the header says.
const MAX_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// What an `Alt-Svc` header field says about HTTP/3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AltSvc {
    /// A value that does not parse: ignored, and what was known of the
    /// origin stays (section 3).
    Malformed,
    /// A valid value with no HTTP/3 the client follows (another protocol,
    /// another host, `ma=0`). It replaces what was cached for the origin
    /// (section 3), so the origin no longer has an HTTP/3 alternative.
    NoH3,
    /// `clear`: the origin takes back what it advertised.
    Clear,
    /// HTTP/3 on the origin's host at this UDP port, for this long.
    H3 { port: u16, max_age: Duration },
}

/// Reads the value of an `Alt-Svc` header field. Several field lines are
/// read as one value, joined with commas.
pub(crate) fn parse(value: &[u8]) -> AltSvc {
    let value = trim(value);
    if value == b"clear" {
        return AltSvc::Clear;
    }
    // The alternatives come in the order the server prefers them.
    let mut found = AltSvc::NoH3;
    for entry in split(value, b',') {
        let mut parts = split(entry, b';');
        let Some((protocol, authority)) = parts.next().and_then(pair) else {
            return AltSvc::Malformed;
        };
        let mut max_age = DEFAULT_MAX_AGE;
        for parameter in parts {
            let Some((name, value)) = pair(parameter) else {
                return AltSvc::Malformed;
            };
            if name == b"ma" {
                let Some(ma) = seconds(unquoted(value)) else {
                    return AltSvc::Malformed;
                };
                max_age = ma;
            }
        }
        // "[host]:port" in quotes; an empty host is the origin's own.
        let Some((host, port)) = authority
            .strip_prefix(b"\"")
            .and_then(|rest| rest.strip_suffix(b"\""))
            .and_then(|inner| {
                let at = inner.iter().rposition(|&b| b == b':')?;
                Some((&inner[..at], port(&inner[at + 1..])?))
            })
        else {
            return AltSvc::Malformed;
        };
        if found == AltSvc::NoH3 && protocol == b"h3" && host.is_empty() && !max_age.is_zero() {
            found = AltSvc::H3 {
                port,
                max_age: max_age.min(MAX_MAX_AGE),
            };
        }
    }
    found
}

fn trim(mut s: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = s {
        s = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = s {
        s = rest;
    }
    s
}

/// The pieces of `s` between `at`, outside quoted strings, trimmed.
fn split(s: &[u8], at: u8) -> impl Iterator<Item = &[u8]> {
    let mut rest = Some(s);
    std::iter::from_fn(move || {
        let s = rest?;
        let mut quoted = false;
        let mut escaped = false;
        for (i, &b) in s.iter().enumerate() {
            match b {
                _ if escaped => escaped = false,
                b'\\' if quoted => escaped = true,
                b'"' => quoted = !quoted,
                _ if b == at && !quoted => {
                    rest = Some(&s[i + 1..]);
                    return Some(trim(&s[..i]));
                }
                _ => {}
            }
        }
        rest = None;
        Some(trim(s))
    })
}

/// `name=value`, with neither empty.
fn pair(s: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = s.iter().position(|&b| b == b'=')?;
    let (name, value) = (&s[..at], &s[at + 1..]);
    (!name.is_empty() && !value.is_empty()).then_some((name, value))
}

fn unquoted(s: &[u8]) -> &[u8] {
    s.strip_prefix(b"\"")
        .and_then(|s| s.strip_suffix(b"\""))
        .unwrap_or(s)
}

fn digits(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 18 || !s.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(s.iter().fold(0, |n, &b| n * 10 + u64::from(b - b'0')))
}

fn seconds(s: &[u8]) -> Option<Duration> {
    digits(s).map(Duration::from_secs)
}

fn port(s: &[u8]) -> Option<u16> {
    let port = u16::try_from(digits(s)?).ok()?;
    (port != 0).then_some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h3(port: u16, secs: u64) -> AltSvc {
        AltSvc::H3 {
            port,
            max_age: Duration::from_secs(secs),
        }
    }

    #[test]
    fn http3_on_the_same_host_is_found() {
        assert_eq!(parse(b"h3=\":443\""), h3(443, 86_400));
        assert_eq!(parse(b"h3=\":8443\"; ma=3600"), h3(8443, 3600));
        assert_eq!(parse(b" h3=\":443\" ;ma=\"60\" ; persist=1 "), h3(443, 60));
        // Among others, in the server's order of preference.
        assert_eq!(
            parse(b"h3-29=\":443\"; ma=10, h3=\":444\"; ma=20, h3=\":445\", h2=\":443\""),
            h3(444, 20)
        );
        // No longer than a week.
        assert_eq!(parse(b"h3=\":443\"; ma=99999999999"), h3(443, 604_800));
        assert_eq!(parse(b"clear"), AltSvc::Clear);
    }

    #[test]
    fn a_valid_value_without_http3_here_says_there_is_none() {
        for value in [
            &b"h2=\":443\""[..],
            // Another host, or one named at all.
            b"h3=\"other.example:443\"",
            b"h3=\"example.com:443\"",
            // Already expired.
            b"h3=\":443\"; ma=0",
            b"h2=\":443\", h3=\"[::1]:443\"",
        ] {
            assert_eq!(
                parse(value),
                AltSvc::NoH3,
                "{}",
                String::from_utf8_lossy(value)
            );
        }
    }

    #[test]
    fn a_value_that_does_not_parse_is_ignored() {
        for value in [
            &b""[..],
            b"h3=:443",
            b"h3=\":0\"",
            b"h3=\":65536\"",
            b"h3=\":44a\"",
            b"h3=\"443\"",
            b"h3=\":443\"; ma=soon",
            b"h3",
            b"h3=\":443\"; ma",
            b"=\":443\"",
            b"Clear",
            b"clear, h3=\":443\"",
            b"h3=\":443\", h2",
        ] {
            assert_eq!(
                parse(value),
                AltSvc::Malformed,
                "{}",
                String::from_utf8_lossy(value)
            );
        }
        // A comma or semicolon inside quotes does not split.
        assert_eq!(parse(b"x=\"a,b;c\\\"d:1\"; ma=5, h3=\":9\""), h3(9, 86_400));
    }

    #[test]
    fn any_bytes_parse_or_are_nothing() {
        let mut s = 0x9e37_79b9_7f4a_7c15u64;
        let alphabet = b"h3=\":;, 0149\\macler-";
        for _ in 0..200_000 {
            let mut value = Vec::new();
            for _ in 0..(s >> 60) + 3 {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                value.push(alphabet[(s % alphabet.len() as u64) as usize]);
            }
            if let AltSvc::H3 { port, max_age } = parse(&value) {
                assert!(port != 0 && !max_age.is_zero() && max_age <= MAX_MAX_AGE);
            }
        }
    }
}
