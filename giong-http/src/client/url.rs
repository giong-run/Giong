//! Absolute `http` and `https` URLs, and resolving a redirect's `Location`
//! against them (RFC 3986).

use std::borrow::Cow;
use std::fmt;
use std::net::Ipv6Addr;
use std::rc::Rc;

/// A URL the client can send a request to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Url {
    pub(crate) https: bool,
    /// Lower case; an IPv6 address without its brackets. Shared with the
    /// URL's [`Origin`], which every request looks its connections up by.
    pub(crate) host: Rc<str>,
    pub(crate) port: u16,
    /// The path and query, starting with `/`; never a fragment.
    pub(crate) target: String,
}

/// Where a connection goes: requests with the same origin share connections.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Origin {
    pub(crate) https: bool,
    pub(crate) host: Rc<str>,
    pub(crate) port: u16,
}

impl Url {
    /// Parses a URL given by the program. Strict: anything that would have
    /// to be guessed at (spaces, non-ASCII, a user name) is an error that
    /// says what to do instead.
    pub(crate) fn parse(input: &str) -> Result<Url, String> {
        parse(input, false)
    }

    /// Resolves a `Location` header sent by a server against this URL.
    /// Lenient where browsers are: spaces and non-ASCII bytes in the path
    /// and query are percent-encoded.
    pub(crate) fn join(&self, location: &str) -> Result<Url, String> {
        let location = location.split('#').next().unwrap_or("");
        if has_scheme(location) {
            return parse(location, true);
        }
        if let Some(rest) = location.strip_prefix("//") {
            let scheme = if self.https { "https" } else { "http" };
            return parse(&format!("{scheme}://{rest}"), true);
        }
        let location = encode_lenient(location);
        let (path, query) = split_query(&location);
        let (base_path, base_query) = split_query(&self.target);
        let target = if path.is_empty() {
            // Same path; a new query if one is given.
            match query {
                Some(q) => format!("{base_path}?{q}"),
                None => match base_query {
                    Some(q) => format!("{base_path}?{q}"),
                    None => base_path.to_owned(),
                },
            }
        } else {
            let merged = if path.starts_with('/') {
                path.to_owned()
            } else {
                let dir = &base_path[..base_path.rfind('/').map_or(0, |i| i + 1)];
                format!("{dir}{path}")
            };
            let mut t = remove_dot_segments(&merged);
            if let Some(q) = query {
                t.push('?');
                t.push_str(q);
            }
            t
        };
        Ok(Url {
            https: self.https,
            host: self.host.clone(),
            port: self.port,
            target,
        })
    }

    pub(crate) fn origin(&self) -> Origin {
        Origin {
            https: self.https,
            host: self.host.clone(),
            port: self.port,
        }
    }

    fn default_port(&self) -> u16 {
        if self.https {
            443
        } else {
            80
        }
    }

    /// Appends the value of the `Host` header: the host (IPv6 in
    /// brackets), and the port unless it is the scheme's default. Written
    /// piece by piece, as [`push_text`](Self::push_text) is: the client
    /// does this for every request, where `format!` costs several times
    /// as much.
    pub(crate) fn push_host_header(&self, out: &mut impl Text) {
        let bracketed = self.host.contains(':');
        if bracketed {
            out.put("[");
        }
        out.put(&self.host);
        if bracketed {
            out.put("]");
        }
        if self.port != self.default_port() {
            out.put(":");
            push_port(out, self.port);
        }
    }

    /// The value of the `Host` header, as HTTP/2's `:authority`: the host
    /// itself, not copied, unless the header needs brackets or a port.
    pub(crate) fn authority(&self) -> Cow<'_, str> {
        if self.port == self.default_port() && !self.host.contains(':') {
            Cow::Borrowed(&self.host)
        } else {
            Cow::Owned(self.host_header())
        }
    }

    /// The value of the `Host` header (see
    /// [`push_host_header`](Self::push_host_header)).
    pub(crate) fn host_header(&self) -> String {
        let mut out = String::with_capacity(self.host.len() + 8);
        self.push_host_header(&mut out);
        out
    }

    /// Appends the URL as text, as `Display` writes it.
    pub(crate) fn push_text(&self, out: &mut impl Text) {
        out.put(if self.https { "https://" } else { "http://" });
        self.push_host_header(out);
        out.put(&self.target);
    }

    /// `"{method} {url}"`, which names a request in the client's errors.
    pub(crate) fn describe(&self, method: &str) -> String {
        let mut out =
            String::with_capacity(method.len() + self.host.len() + self.target.len() + 18);
        out.push_str(method);
        out.push(' ');
        self.push_text(&mut out);
        out
    }

    /// The URL as text, as `Display` writes it.
    pub(crate) fn text(&self) -> String {
        let mut out = String::with_capacity(self.host.len() + self.target.len() + 16);
        self.push_text(&mut out);
        out
    }
}

/// Text the URL's pieces are appended to: a `String`, or the bytes of a
/// request head.
pub(crate) trait Text {
    fn put(&mut self, s: &str);
}

impl Text for String {
    fn put(&mut self, s: &str) {
        self.push_str(s);
    }
}

impl Text for Vec<u8> {
    fn put(&mut self, s: &str) {
        self.extend_from_slice(s.as_bytes());
    }
}

fn push_port(out: &mut impl Text, mut port: u16) {
    let mut digits = [0u8; 5];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (port % 10) as u8;
        port /= 10;
        if port == 0 {
            break;
        }
    }
    // ASCII digits.
    out.put(std::str::from_utf8(&digits[at..]).unwrap_or_default());
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text())
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.https { "https" } else { "http" };
        if self.host.contains(':') {
            write!(f, "{scheme}://[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{scheme}://{}:{}", self.host, self.port)
        }
    }
}

/// Whether `s` starts with `scheme:` (RFC 3986 section 3.1).
fn has_scheme(s: &str) -> bool {
    let Some(colon) = s.find(':') else {
        return false;
    };
    let scheme = &s.as_bytes()[..colon];
    scheme.first().is_some_and(u8::is_ascii_alphabetic)
        && scheme
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
}

fn split_query(target: &str) -> (&str, Option<&str>) {
    match target.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (target, None),
    }
}

/// Percent-encodes what may not appear in a request target: controls,
/// spaces, and bytes outside ASCII.
fn encode_lenient(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if (0x21..=0x7e).contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// RFC 3986 section 5.2.4.
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut segments = path.split('/').peekable();
    // A path that starts with `/` gives an empty first segment.
    if path.starts_with('/') {
        segments.next();
    }
    let mut trailing_slash = false;
    while let Some(seg) = segments.next() {
        let last = segments.peek().is_none();
        match seg {
            "." => trailing_slash = last,
            ".." => {
                out.pop();
                trailing_slash = last;
            }
            _ => {
                out.push(seg);
                trailing_slash = false;
            }
        }
    }
    let mut result = String::with_capacity(path.len() + 1);
    for seg in &out {
        result.push('/');
        result.push_str(seg);
    }
    if trailing_slash || result.is_empty() {
        result.push('/');
    }
    result
}

/// `host` in lower case, copied once.
fn lowercase(host: &str) -> Rc<str> {
    if host.bytes().any(|b| b.is_ascii_uppercase()) {
        Rc::from(host.to_ascii_lowercase())
    } else {
        Rc::from(host)
    }
}

/// Where `byte` first appears in `s`. The searches here look for ASCII
/// bytes, which never occur inside a multi-byte UTF-8 sequence, so every
/// position found is a char boundary. Byte loops: `str`'s pattern searches
/// cost several times as much on strings this short, and every request
/// parses its URL.
fn find(s: &str, byte: u8) -> Option<usize> {
    s.bytes().position(|b| b == byte)
}

fn parse(input: &str, lenient: bool) -> Result<Url, String> {
    let (scheme, rest) = match input.as_bytes().windows(3).position(|w| w == b"://") {
        Some(at) => (&input[..at], &input[at + 3..]),
        None => {
            return Err(format!(
                "{input:?} is not an absolute URL; it must start with http:// or https://"
            ))
        }
    };
    let https = if scheme.eq_ignore_ascii_case("https") {
        true
    } else if scheme.eq_ignore_ascii_case("http") {
        false
    } else {
        return Err(format!(
            "{input:?} has the scheme {scheme:?}; only http and https URLs are supported"
        ));
    };
    let rest = &rest[..find(rest, b'#').unwrap_or(rest.len())];
    let end = rest
        .bytes()
        .position(|b| b == b'/' || b == b'?')
        .unwrap_or(rest.len());
    let (authority, target) = rest.split_at(end);
    if find(authority, b'@').is_some() {
        return Err(format!(
            "{input:?} has a user name or password in it; they are not sent from a URL: \
             set an Authorization header instead"
        ));
    }
    let (host, port) = if let Some(after) = authority.strip_prefix('[') {
        let (v6, after) = after
            .split_once(']')
            .ok_or_else(|| format!("{input:?} has an IPv6 address without its closing ]"))?;
        let addr: Ipv6Addr = v6.parse().map_err(|_| {
            format!("{input:?} has {v6:?} in brackets, which is not an IPv6 address")
        })?;
        let port = match after {
            "" => None,
            p => Some(
                p.strip_prefix(':')
                    .ok_or_else(|| format!("{input:?} has {p:?} after the IPv6 address"))?,
            ),
        };
        (Rc::from(addr.to_string()), port)
    } else {
        let (host, port) = match authority.bytes().rposition(|b| b == b':') {
            Some(colon) => (&authority[..colon], Some(&authority[colon + 1..])),
            None => (authority, None),
        };
        if host.is_empty() {
            return Err(format!("{input:?} has no host"));
        }
        if !host.is_ascii() {
            return Err(format!(
                "{input:?} has a host name that is not ASCII; use its punycode form (xn--...)"
            ));
        }
        if !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        {
            return Err(format!(
                "{input:?} has {host:?} as its host, which is not a host name or an IPv4 address"
            ));
        }
        (lowercase(host), port)
    };
    let port = match port {
        None | Some("") => {
            if https {
                443
            } else {
                80
            }
        }
        Some(p) => match p.parse::<u16>() {
            Ok(n) if n > 0 && p.bytes().all(|b| b.is_ascii_digit()) => n,
            _ => {
                return Err(format!(
                    "{input:?} has {p:?} as its port; it must be 1 to 65535"
                ))
            }
        },
    };
    let target = if lenient {
        encode_lenient(target)
    } else {
        if let Some(b) = target.bytes().find(|b| !(0x21..=0x7e).contains(b)) {
            return Err(format!(
                "{input:?} has {:?} in its path or query; percent-encode it (a space is %20)",
                b as char
            ));
        }
        target.to_owned()
    };
    let target = match target.as_bytes().first() {
        None => "/".to_owned(),
        Some(b'?') => format!("/{target}"),
        _ => target,
    };
    Ok(Url {
        https,
        host,
        port,
        target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn urls_are_read() {
        let u = url("HTTPS://API.Example.com/v1/users?id=7#top");
        assert!(u.https);
        assert_eq!(&*u.host, "api.example.com");
        assert_eq!(u.port, 443);
        assert_eq!(u.target, "/v1/users?id=7");
        assert_eq!(u.host_header(), "api.example.com");
        assert_eq!(u.to_string(), "https://api.example.com/v1/users?id=7");

        let u = url("http://127.0.0.1:8080");
        assert_eq!((u.https, u.port, u.target.as_str()), (false, 8080, "/"));
        assert_eq!(u.host_header(), "127.0.0.1:8080");

        let u = url("http://[::1]:3000?x=1");
        assert_eq!(&*u.host, "::1");
        assert_eq!(u.target, "/?x=1");
        assert_eq!(u.host_header(), "[::1]:3000");
        assert_eq!(u.to_string(), "http://[::1]:3000/?x=1");
        assert_eq!(url("http://h:/").port, 80);
        assert_eq!(&*url("http://my_host.local/").host, "my_host.local");
    }

    #[test]
    fn the_authority_is_borrowed_unless_it_needs_a_port_or_brackets() {
        let u = url("https://api.example.com/x");
        assert!(matches!(u.authority(), Cow::Borrowed("api.example.com")));
        assert_eq!(url("http://127.0.0.1:8080/").authority(), "127.0.0.1:8080");
        assert_eq!(url("https://[::1]/").authority(), "[::1]");
        assert_eq!(url("https://example.com:443/").authority(), "example.com");
        // The host is shared with the origin, not copied.
        assert!(Rc::ptr_eq(&u.host, &u.origin().host));
    }

    #[test]
    fn bad_urls_say_what_to_do() {
        for (input, says) in [
            ("example.com/x", "must start with http:// or https://"),
            ("ftp://example.com/", "only http and https"),
            ("https://user:pw@example.com/", "Authorization header"),
            ("https:///x", "no host"),
            ("https://exämple.com/", "punycode"),
            ("https://a b/", "not a host name"),
            ("https://example.com:0/", "1 to 65535"),
            ("https://example.com:99999/", "1 to 65535"),
            ("https://example.com:+80/", "1 to 65535"),
            ("https://[::1/", "closing ]"),
            ("https://[nope]/", "not an IPv6 address"),
            ("https://[::1]x/", "after the IPv6 address"),
            ("https://example.com/a b", "percent-encode"),
            ("https://example.com/é", "percent-encode"),
        ] {
            let e = Url::parse(input).unwrap_err();
            assert!(e.contains(says), "{input}: {e}");
        }
    }

    #[test]
    fn locations_are_resolved_as_rfc_3986_says() {
        // The examples of RFC 3986 section 5.4 that apply to HTTP.
        let base = url("http://a/b/c/d;p?q");
        for (rel, want) in [
            ("g", "http://a/b/c/g"),
            ("./g", "http://a/b/c/g"),
            ("g/", "http://a/b/c/g/"),
            ("/g", "http://a/g"),
            ("//g", "http://g/"),
            ("?y", "http://a/b/c/d;p?y"),
            ("g?y", "http://a/b/c/g?y"),
            ("#s", "http://a/b/c/d;p?q"),
            ("g#s", "http://a/b/c/g"),
            (";x", "http://a/b/c/;x"),
            ("", "http://a/b/c/d;p?q"),
            (".", "http://a/b/c/"),
            ("./", "http://a/b/c/"),
            ("..", "http://a/b/"),
            ("../", "http://a/b/"),
            ("../g", "http://a/b/g"),
            ("../..", "http://a/"),
            ("../../g", "http://a/g"),
            ("../../../g", "http://a/g"),
            ("/./g", "http://a/g"),
            ("/../g", "http://a/g"),
            ("g.", "http://a/b/c/g."),
            ("..g", "http://a/b/c/..g"),
            ("./../g", "http://a/b/g"),
            ("g/./h", "http://a/b/c/g/h"),
            ("g/../h", "http://a/b/c/h"),
            ("https://other:8443/x", "https://other:8443/x"),
        ] {
            assert_eq!(base.join(rel).unwrap().to_string(), want, "{rel:?}");
        }
        let secure = url("https://a/x");
        assert_eq!(secure.join("//b/y").unwrap().to_string(), "https://b/y");
        // Servers send unencoded bytes; they are encoded, not refused.
        assert_eq!(
            secure.join("/a b/é?q=ü").unwrap().target,
            "/a%20b/%C3%A9?q=%C3%BC"
        );
        assert!(secure.join("ftp://x/").is_err());
    }
}
