//! HTTP proxies: which requests go through one, and how they are sent.
//!
//! A request to an `http` URL goes to the proxy with the whole URL as its
//! target (`GET http://host/path HTTP/1.1`). A request to an `https` URL
//! asks the proxy for a tunnel (`CONNECT host:443`) and does its TLS
//! handshake with the server through it, so the proxy sees neither the
//! request nor the response.

use std::fmt;
use std::net::IpAddr;

use super::error::Error;
use super::url::{Origin, Url};

/// Which proxy requests go through, and which hosts are reached directly.
///
/// ```
/// use giong_http::client::{Client, Proxy};
///
/// # fn main() -> Result<(), giong_http::client::Error> {
/// // Everything through one proxy, except the cluster's own services.
/// let proxy = Proxy::all("http://user:secret@proxy.internal:3128")?
///     .no_proxy("localhost, 127.0.0.1, .svc.cluster.local, 10.0.0.0/8")?;
/// let client = Client::builder().proxy(proxy).build();
///
/// // Or what the environment says (HTTPS_PROXY, HTTP_PROXY, NO_PROXY...).
/// let mut builder = Client::builder();
/// if let Some(proxy) = Proxy::from_env()? {
///     builder = builder.proxy(proxy);
/// }
/// # let _ = (client, builder);
/// # Ok(())
/// # }
/// ```
///
/// Only plain `http://` proxies: TLS to the proxy itself, and SOCKS, are
/// not supported yet. A user name and password in the proxy's URL are sent
/// to the proxy (`Proxy-Authorization: Basic`), never to the server.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Proxy {
    http: Option<Server>,
    https: Option<Server>,
    no_proxy: Vec<Rule>,
}

/// A proxy server: where to connect, and the credentials to give it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Server {
    pub(crate) host: String,
    pub(crate) port: u16,
    /// The value of `Proxy-Authorization`, when the URL has a user name.
    pub(crate) authorization: Option<String>,
}

impl fmt::Debug for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")?;
        if self.authorization.is_some() {
            f.write_str(" (with credentials)")?;
        }
        Ok(())
    }
}

impl fmt::Display for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "http://[{}]:{}", self.host, self.port)
        } else {
            write!(f, "http://{}:{}", self.host, self.port)
        }
    }
}

/// Where connections for a request go: straight to its origin, or through
/// a proxy. Requests with the same route share connections.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Route {
    pub(crate) origin: Origin,
    pub(crate) proxy: Option<Server>,
}

impl Route {
    /// Whether requests on this route are sent to the proxy with the whole
    /// URL as their target (`http` through a proxy). `https` goes through a
    /// tunnel and is sent as it would be to the server.
    pub(crate) fn absolute_form(&self) -> Option<&Server> {
        self.proxy.as_ref().filter(|_| !self.origin.https)
    }
}

/// One entry of a `NO_PROXY` list.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Rule {
    /// `*`: no request goes through the proxy.
    All,
    /// A host name and the names under it (`example.com`, `.example.com`
    /// and `*.example.com` all match `example.com` and `api.example.com`).
    Domain(String, Option<u16>),
    /// One address.
    Ip(IpAddr, Option<u16>),
    /// A range of addresses (`10.0.0.0/8`, `fd00::/8`).
    Net(IpAddr, u8),
}

impl Proxy {
    /// Every request goes through the proxy at `url` (`http://host:port`,
    /// with `user:password@` if it asks for them).
    pub fn all(url: &str) -> Result<Proxy, Error> {
        let server = parse_server(url, false)?;
        Ok(Proxy {
            http: Some(server.clone()),
            https: Some(server),
            no_proxy: Vec::new(),
        })
    }

    /// Requests to `http` URLs go through the proxy at `url`; `https` ones
    /// do not. Combine with [`Proxy::https`] by giving both to
    /// [`ClientBuilder::proxy`](super::ClientBuilder::proxy).
    pub fn http(url: &str) -> Result<Proxy, Error> {
        Ok(Proxy {
            http: Some(parse_server(url, false)?),
            ..Proxy::default()
        })
    }

    /// Requests to `https` URLs go through the proxy at `url` (a tunnel:
    /// the proxy sees only the host name); `http` ones do not.
    pub fn https(url: &str) -> Result<Proxy, Error> {
        Ok(Proxy {
            https: Some(parse_server(url, false)?),
            ..Proxy::default()
        })
    }

    /// Hosts reached without the proxy, in the format of `NO_PROXY`: a
    /// list separated by commas of host names (which also cover the names
    /// under them: `example.com` covers `api.example.com`), addresses,
    /// ranges of addresses (`10.0.0.0/8`), any of them with `:port`, or
    /// `*` for every host.
    ///
    /// Nothing is left out unless listed: `localhost` and `127.0.0.1` too
    /// go through the proxy unless they are here.
    pub fn no_proxy(mut self, list: &str) -> Result<Proxy, Error> {
        for entry in list.split([',', ' ', '\t', '\n']) {
            if !entry.is_empty() {
                self.no_proxy.push(parse_rule(entry)?);
            }
        }
        Ok(self)
    }

    /// The proxy the environment asks for, the way curl and most tools
    /// read it, or `None` when it asks for none:
    ///
    /// * `https_proxy` (or `HTTPS_PROXY`) for `https` URLs and `http_proxy`
    ///   (or `HTTP_PROXY`) for `http` ones; `all_proxy` (or `ALL_PROXY`)
    ///   for either when its own is not set. A value without a scheme
    ///   (`proxy:3128`) means `http://`. The lower case name wins.
    /// * `no_proxy` (or `NO_PROXY`), as [`Proxy::no_proxy`] reads it.
    /// * `HTTP_PROXY` in upper case is ignored when `REQUEST_METHOD` is
    ///   set: in a CGI program, a request's `Proxy:` header arrives as
    ///   `HTTP_PROXY` ("httpoxy").
    ///
    /// A value that is not a valid proxy URL is an error naming the
    /// variable, not something quietly skipped.
    pub fn from_env() -> Result<Option<Proxy>, Error> {
        from_lookup(|name| std::env::var(name).ok())
    }

    /// The route to `url`'s origin: through the proxy for its scheme,
    /// unless the host is listed in `no_proxy`.
    pub(crate) fn route(&self, url: &Url) -> Route {
        let origin = url.origin();
        let server = if url.https { &self.https } else { &self.http };
        let proxy = match server {
            Some(server) if !self.bypass(&origin.host, origin.port) => Some(server.clone()),
            _ => None,
        };
        Route { origin, proxy }
    }

    /// Adds `other`'s settings: its proxies replace these for the schemes it
    /// sets, and its `no_proxy` entries are added.
    pub(crate) fn merge(&mut self, other: Proxy) {
        if other.http.is_some() {
            self.http = other.http;
        }
        if other.https.is_some() {
            self.https = other.https;
        }
        self.no_proxy.extend(other.no_proxy);
    }

    fn bypass(&self, host: &str, port: u16) -> bool {
        let ip: Option<IpAddr> = host.parse().ok();
        self.no_proxy.iter().any(|rule| match rule {
            Rule::All => true,
            Rule::Domain(name, p) => {
                !matches!(p, Some(q) if *q != port)
                    && (host == name
                        || (host.len() > name.len()
                            && host.ends_with(name.as_str())
                            && host.as_bytes()[host.len() - name.len() - 1] == b'.'))
            }
            Rule::Ip(addr, p) => !matches!(p, Some(q) if *q != port) && ip == Some(*addr),
            Rule::Net(net, bits) => ip.is_some_and(|ip| in_net(ip, *net, *bits)),
        })
    }
}

impl fmt::Debug for Proxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Proxy")
            .field("http", &self.http)
            .field("https", &self.https)
            .field("no_proxy", &self.no_proxy.len())
            .finish()
    }
}

/// [`Proxy::from_env`] with the variables read through `get`.
pub(crate) fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Proxy>, Error> {
    let read = |lower: &str, upper: &str| -> Option<(String, String)> {
        for name in [lower, upper] {
            if name == "HTTP_PROXY" && get("REQUEST_METHOD").is_some() {
                continue;
            }
            if let Some(v) = get(name).filter(|v| !v.trim().is_empty()) {
                return Some((name.to_owned(), v.trim().to_owned()));
            }
        }
        None
    };
    let server = |found: Option<(String, String)>| -> Result<Option<Server>, Error> {
        match found {
            None => Ok(None),
            Some((name, value)) => parse_server(&value, true).map(Some).map_err(|e| {
                Error::request(format!("the environment variable {name}: {}", message(&e)))
            }),
        }
    };
    let all = read("all_proxy", "ALL_PROXY");
    let http = server(read("http_proxy", "HTTP_PROXY").or_else(|| all.clone()))?;
    let https = server(read("https_proxy", "HTTPS_PROXY").or(all))?;
    if http.is_none() && https.is_none() {
        return Ok(None);
    }
    let mut proxy = Proxy {
        http,
        https,
        no_proxy: Vec::new(),
    };
    if let Some((name, list)) = read("no_proxy", "NO_PROXY") {
        proxy = proxy.no_proxy(&list).map_err(|e| {
            Error::request(format!("the environment variable {name}: {}", message(&e)))
        })?;
    }
    Ok(Some(proxy))
}

/// An error's message without its code and link, to wrap in another.
fn message(e: &Error) -> String {
    let s = e.to_string();
    let s = s.split_once(": ").map_or(s.as_str(), |(_, m)| m);
    s.rsplit_once(" See ").map_or(s, |(m, _)| m).to_owned()
}

/// Reads a proxy's URL: `http://[user[:password]@]host[:port][/]`. From the
/// environment (`lenient`), a URL without a scheme means `http://`.
fn parse_server(input: &str, lenient: bool) -> Result<Server, Error> {
    let bad = |why: &str| {
        Error::request(format!(
            "{:?} is not a proxy URL the client can use: {why}",
            redact(input)
        ))
    };
    let rest =
        match input.split_once("://") {
            Some((scheme, rest)) => match scheme.to_ascii_lowercase().as_str() {
                "http" => rest,
                "https" => return Err(bad(
                    "TLS to the proxy itself (https://) is not supported yet; most proxies also \
                     accept http://, and requests to https URLs are still encrypted end to end \
                     through the proxy's tunnel",
                )),
                s if s.starts_with("socks") => {
                    return Err(bad(
                        "SOCKS proxies are not supported yet; use an HTTP proxy",
                    ))
                }
                _ => return Err(bad("the scheme must be http://")),
            },
            None if lenient => input,
            None => return Err(bad("it needs its scheme, as in http://proxy.internal:3128")),
        };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    if !matches!(tail, "" | "/") {
        return Err(bad("a proxy URL has no path, query or fragment"));
    }
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let url = Url::parse(&format!("http://{hostport}/")).map_err(|m| bad(&m))?;
    let authorization = match userinfo {
        None => None,
        Some(userinfo) => {
            let (user, password) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            let user = percent_decode(user).ok_or_else(|| bad("bad %-escape in the user name"))?;
            let password =
                percent_decode(password).ok_or_else(|| bad("bad %-escape in the password"))?;
            if user.is_empty() {
                return Err(bad("the user name is empty"));
            }
            let mut pair = user;
            pair.push(b':');
            pair.extend_from_slice(&password);
            Some(format!("Basic {}", base64(&pair)))
        }
    };
    Ok(Server {
        host: url.host,
        port: url.port,
        authorization,
    })
}

/// `input` with the password replaced, for messages.
fn redact(input: &str) -> String {
    let start = input.find("://").map_or(0, |i| i + 3);
    let end = input[start..]
        .find(['/', '?', '#'])
        .map_or(input.len(), |i| start + i);
    match input[start..end].rfind('@') {
        Some(at) => {
            let userinfo = &input[start..start + at];
            let user = userinfo.split(':').next().unwrap_or("");
            format!("{}{user}:***{}", &input[..start], &input[start + at..])
        }
        None => input.to_owned(),
    }
}

fn parse_rule(entry: &str) -> Result<Rule, Error> {
    let bad = |why: &str| Error::request(format!("{entry:?} in the no-proxy list: {why}"));
    if entry == "*" {
        return Ok(Rule::All);
    }
    if let Some((addr, bits)) = entry.split_once('/') {
        let addr: IpAddr = addr
            .trim_matches(['[', ']'])
            .parse()
            .map_err(|_| bad("a range is an address and a prefix length, as in 10.0.0.0/8"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let bits: u8 =
            bits.parse().ok().filter(|&b| b <= max).ok_or_else(|| {
                bad("the prefix length is not a number from 0 to 32 (128 for IPv6)")
            })?;
        return Ok(Rule::Net(addr, bits));
    }
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Ok(Rule::Ip(ip, None));
    }
    let (host, port) = if let Some(rest) = entry.strip_prefix('[') {
        let (ip, after) = rest
            .split_once(']')
            .ok_or_else(|| bad("an IPv6 address opened with [ is not closed"))?;
        let ip: IpAddr = ip.parse().map_err(|_| bad("not an IPv6 address"))?;
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p.parse().map_err(|_| bad("the port is not a number"))?),
            None if after.is_empty() => None,
            None => return Err(bad("unexpected text after the address")),
        };
        return Ok(Rule::Ip(ip, port));
    } else {
        match entry.rsplit_once(':') {
            Some((h, p)) => (
                h,
                Some(
                    p.parse::<u16>()
                        .map_err(|_| bad("the port is not a number"))?,
                ),
            ),
            None => (entry, None),
        }
    };
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(Rule::Ip(ip, port));
    }
    let name = host
        .strip_prefix("*.")
        .or_else(|| host.strip_prefix('.'))
        .unwrap_or(host)
        .to_ascii_lowercase();
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
    {
        return Err(bad(
            "not a host name, an address or a range (only a leading `*.` is understood as a pattern)",
        ));
    }
    Ok(Rule::Domain(name, port))
}

fn in_net(ip: IpAddr, net: IpAddr, bits: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(a) & mask == u32::from(b) & mask
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(a) & mask == u128::from(b) & mask
        }
        _ => false,
    }
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    Some(out)
}

fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for group in input.chunks(3) {
        let n = (u32::from(group[0]) << 16)
            | (u32::from(*group.get(1).unwrap_or(&0)) << 8)
            | u32::from(*group.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= group.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn via(proxy: &Proxy, u: &str) -> Option<String> {
        proxy.route(&url(u)).proxy.map(|s| s.to_string())
    }

    #[test]
    fn base64_matches_the_rfc_4648_examples() {
        for (input, out) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), out);
        }
    }

    #[test]
    fn proxy_urls_are_read_strictly() {
        let s = parse_server("http://u%40x:p%3Aw@Proxy.Internal:3128/", false).unwrap();
        assert_eq!((s.host.as_str(), s.port), ("proxy.internal", 3128));
        assert_eq!(s.authorization.as_deref(), Some("Basic dUB4OnA6dw=="));
        let s = parse_server("http://[::1]", false).unwrap();
        assert_eq!(
            (s.host.as_str(), s.port, s.authorization.is_none()),
            ("::1", 80, true)
        );
        assert_eq!(s.to_string(), "http://[::1]:80");
        assert_eq!(parse_server("proxy:8080", true).unwrap().port, 8080);

        for (input, says) in [
            ("proxy:8080", "needs its scheme"),
            ("https://proxy", "not supported yet"),
            ("socks5://proxy:1080", "SOCKS"),
            ("ftp://proxy", "must be http"),
            ("http://proxy/path", "no path"),
            ("http://:pw@proxy", "user name is empty"),
            ("http://u:%zz@proxy", "%-escape"),
        ] {
            let e = parse_server(input, false).unwrap_err().to_string();
            assert!(e.contains(says), "{input}: {e}");
        }
        // The password never shows in a message or in Debug.
        let e = parse_server("http://me:hunter2@proxy/x", false).unwrap_err();
        assert!(!e.to_string().contains("hunter2"), "{e}");
        assert!(e.to_string().contains("me:***@proxy"), "{e}");
        let p = Proxy::all("http://me:hunter2@proxy").unwrap();
        let debug = format!("{p:?}");
        assert!(
            !debug.contains("hunter2") && !debug.contains("Basic"),
            "{debug}"
        );
    }

    #[test]
    fn the_scheme_picks_the_proxy_and_no_proxy_bypasses_it() {
        let p = Proxy::all("http://proxy:3128")
            .unwrap()
            .no_proxy("localhost, .svc.cluster.local,*.corp.example ,10.0.0.0/8, 192.168.1.5, example.org:8080, [::1], fd00::/8")
            .unwrap();
        let proxied = Some("http://proxy:3128".to_owned());
        assert_eq!(via(&p, "http://api.example.com/"), proxied);
        assert_eq!(via(&p, "https://api.example.com/"), proxied);
        for direct in [
            "http://localhost:8080/",
            "http://db.ns.svc.cluster.local/",
            "http://svc.cluster.local/",
            "https://a.b.corp.example/",
            "https://corp.example/",
            "http://10.1.2.3/",
            "http://192.168.1.5:9000/",
            "http://example.org:8080/",
            "http://[::1]:80/",
            "http://[fd12::1]/",
        ] {
            assert_eq!(via(&p, direct), None, "{direct}");
        }
        for proxied_url in [
            "http://notlocalhost/",
            "http://xsvc.cluster.local/",
            "http://11.0.0.1/",
            "http://192.168.1.6/",
            "http://example.org/",
            "http://127.0.0.1/",
        ] {
            assert_eq!(via(&p, proxied_url), proxied, "{proxied_url}");
        }

        let star = Proxy::all("http://proxy").unwrap().no_proxy("*").unwrap();
        assert_eq!(via(&star, "https://anything/"), None);

        let mut both = Proxy::http("http://plain:1").unwrap();
        both.merge(Proxy::https("http://tunnel:2").unwrap());
        assert_eq!(via(&both, "http://x/").as_deref(), Some("http://plain:1"));
        assert_eq!(via(&both, "https://x/").as_deref(), Some("http://tunnel:2"));
        assert_eq!(via(&Proxy::http("http://p").unwrap(), "https://x/"), None);

        for bad in [
            "10.0.0.0/33",
            "a/b",
            "exa!mple",
            "host:port",
            "[::1",
            "a*b.com",
        ] {
            assert!(Proxy::default().no_proxy(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_environment_is_read_the_way_curl_reads_it() {
        let env = |pairs: &[(&str, &str)]| {
            let map: HashMap<String, String> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            from_lookup(move |k| map.get(k).cloned())
        };
        assert_eq!(env(&[]).unwrap(), None);
        assert_eq!(
            env(&[("NO_PROXY", "*"), ("HTTPS_PROXY", " ")]).unwrap(),
            None
        );

        let p = env(&[
            ("HTTPS_PROXY", "secure:3128"),
            ("http_proxy", "http://lower:1"),
            ("HTTP_PROXY", "http://upper:2"),
            ("NO_PROXY", "internal"),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(via(&p, "https://x/").as_deref(), Some("http://secure:3128"));
        assert_eq!(via(&p, "http://x/").as_deref(), Some("http://lower:1"));
        assert_eq!(via(&p, "http://internal/"), None);

        let p = env(&[("ALL_PROXY", "http://all:9"), ("https_proxy", "http://s:1")])
            .unwrap()
            .unwrap();
        assert_eq!(via(&p, "http://x/").as_deref(), Some("http://all:9"));
        assert_eq!(via(&p, "https://x/").as_deref(), Some("http://s:1"));

        // httpoxy: in a CGI program, HTTP_PROXY may come from the request.
        let p = env(&[("HTTP_PROXY", "http://evil:1"), ("REQUEST_METHOD", "GET")]).unwrap();
        assert_eq!(p, None);

        let e = env(&[("HTTPS_PROXY", "socks5://x:1")])
            .unwrap_err()
            .to_string();
        assert!(e.contains("HTTPS_PROXY") && e.contains("SOCKS"), "{e}");
        let e = env(&[("HTTP_PROXY", "http://p"), ("no_proxy", "10.0.0.0/99")])
            .unwrap_err()
            .to_string();
        assert!(e.contains("no_proxy") && e.contains("prefix length"), "{e}");
    }
}
