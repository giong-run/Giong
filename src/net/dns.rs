//! Name resolution that does not block the worker: `/etc/hosts`, then DNS over
//! UDP (and TCP when an answer does not fit) to the servers in
//! `/etc/resolv.conf`, the way musl's resolver does it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::future::{poll_fn, Future};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, SystemTime};

use crate::error::{raise, Code};
use crate::net::{TcpStream, UdpSocket};
use crate::time::{timeout, Instant};

const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

const RCODE_NOERROR: u8 = 0;
const RCODE_NXDOMAIN: u8 = 3;

/// Longest a positive answer is kept in the cache, whatever its TTL says.
const MAX_CACHE_TTL: Duration = Duration::from_secs(300);
/// Entries kept in one resolver's cache.
const MAX_CACHE_ENTRIES: usize = 1024;
/// How often the system resolver checks whether its files changed.
const RELOAD_CHECK: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- config

/// How a [`Resolver`] finds names: the settings of `/etc/resolv.conf` and the
/// entries of `/etc/hosts`.
#[derive(Clone, Debug)]
pub struct ResolverConfig {
    nameservers: Vec<SocketAddr>,
    search: Vec<String>,
    ndots: usize,
    timeout: Duration,
    attempts: usize,
    rotate: bool,
    hosts: Vec<(String, IpAddr)>,
}

impl Default for ResolverConfig {
    /// No search domains, `ndots:1`, `timeout:5`, `attempts:2`, and the
    /// name server `127.0.0.1`, as glibc and musl do without a
    /// `resolv.conf`.
    fn default() -> Self {
        Self {
            nameservers: vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 53)],
            search: Vec::new(),
            ndots: 1,
            timeout: Duration::from_secs(5),
            attempts: 2,
            rotate: false,
            hosts: Vec::new(),
        }
    }
}

impl ResolverConfig {
    /// Reads `/etc/resolv.conf` and `/etc/hosts`. A missing file counts as
    /// empty.
    pub fn system() -> io::Result<Self> {
        let resolv = read_optional("/etc/resolv.conf")?;
        let hosts = read_optional("/etc/hosts")?;
        Ok(Self::parse(&resolv, &hosts))
    }

    /// Builds a configuration from the text of a `resolv.conf` and a `hosts`
    /// file. Understands `nameserver` (up to three), `search`, `domain` and
    /// the options `ndots`, `timeout`, `attempts` and `rotate`; everything else
    /// is ignored.
    pub fn parse(resolv_conf: &str, hosts: &str) -> Self {
        let mut cfg = Self {
            nameservers: Vec::new(),
            ..Self::default()
        };
        for line in resolv_conf.lines() {
            let line = line.split(['#', ';']).next().unwrap_or("");
            let mut words = line.split_whitespace();
            match words.next() {
                Some("nameserver") => {
                    // A zone index (`fe80::1%eth0`) is not supported; skip it.
                    if let Some(ip) = words.next().and_then(|w| w.parse::<IpAddr>().ok()) {
                        if cfg.nameservers.len() < 3 {
                            cfg.nameservers.push(SocketAddr::new(ip, 53));
                        }
                    }
                }
                Some("search") => {
                    cfg.search = words.map(|d| d.trim_end_matches('.').to_string()).collect();
                    cfg.search.retain(|d| !d.is_empty());
                }
                Some("domain") => {
                    cfg.search = words
                        .next()
                        .map(|d| d.trim_end_matches('.').to_string())
                        .filter(|d| !d.is_empty())
                        .into_iter()
                        .collect();
                }
                Some("options") => {
                    for option in words {
                        let (key, value) = option.split_once(':').unwrap_or((option, ""));
                        let number = value.parse::<usize>().ok();
                        match (key, number) {
                            ("ndots", Some(n)) => cfg.ndots = n.min(15),
                            ("timeout", Some(n)) => {
                                cfg.timeout = Duration::from_secs(n.clamp(1, 30) as u64)
                            }
                            ("attempts", Some(n)) => cfg.attempts = n.clamp(1, 5),
                            ("rotate", _) => cfg.rotate = true,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        if cfg.nameservers.is_empty() {
            cfg.nameservers = Self::default().nameservers;
        }
        cfg.hosts = parse_hosts(hosts);
        cfg
    }

    /// Uses these name servers instead of the configured ones. A server
    /// without a port is reached on port 53.
    pub fn nameservers(mut self, servers: impl IntoIterator<Item = SocketAddr>) -> Self {
        self.nameservers = servers.into_iter().collect();
        self
    }

    /// How long to wait for one server's answer before asking the next one.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How many rounds through all servers before giving up.
    pub fn attempts(mut self, attempts: usize) -> Self {
        self.attempts = attempts.max(1);
        self
    }

    /// The configured name servers.
    pub fn nameserver_addrs(&self) -> &[SocketAddr] {
        &self.nameservers
    }

    /// The configured search domains.
    pub fn search_domains(&self) -> &[String] {
        &self.search
    }

    /// The names to ask for, in order, following `ndots` and the search list.
    fn candidates(&self, name: &str) -> Vec<String> {
        if let Some(absolute) = name.strip_suffix('.') {
            return vec![absolute.to_string()];
        }
        let dots = name.matches('.').count();
        let mut out = Vec::with_capacity(self.search.len() + 1);
        if dots >= self.ndots {
            out.push(name.to_string());
        }
        for domain in &self.search {
            out.push(format!("{name}.{domain}"));
        }
        if dots < self.ndots {
            out.push(name.to_string());
        }
        out
    }

    fn hosts_lookup(&self, name: &str) -> Vec<IpAddr> {
        let name = name.strip_suffix('.').unwrap_or(name);
        self.hosts
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, ip)| *ip)
            .collect()
    }
}

/// The addresses of a `localhost` name (RFC 6761, section 6.3): the loopback
/// addresses the hosts file gives it, or `::1` and `127.0.0.1`.
///
/// Such a name is never sent to a DNS server. Database clients treat
/// `localhost` as this machine, connecting without TLS and sending passwords
/// they would not send over a network; a search domain with a wildcard record,
/// or a forged answer, must not be able to point it at another machine. For
/// the same reason a hosts file entry that is not a loopback address is
/// ignored.
fn localhost(mut from_hosts: Vec<IpAddr>) -> Vec<IpAddr> {
    from_hosts.retain(is_loopback);
    if from_hosts.is_empty() {
        vec![Ipv6Addr::LOCALHOST.into(), Ipv4Addr::LOCALHOST.into()]
    } else {
        from_hosts
    }
}

fn is_loopback(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

fn read_optional(path: &str) -> io::Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

fn parse_hosts(text: &str) -> Vec<(String, IpAddr)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("");
        let mut words = line.split_whitespace();
        let Some(ip) = words.next().and_then(|w| w.parse::<IpAddr>().ok()) else {
            continue;
        };
        for name in words {
            out.push((name.trim_end_matches('.').to_ascii_lowercase(), ip));
        }
    }
    out
}

// --------------------------------------------------------------- resolver

/// Resolves host names without blocking the worker.
///
/// Looks in the hosts file first, then asks the DNS servers: `A` and `AAAA`
/// queries go out together over UDP, an answer marked as truncated is asked
/// again over TCP, and the search list is applied as `ndots` says. Answers
/// are cached for their TTL (at most five minutes).
///
/// A resolver belongs to the worker that created it and is cheap to clone.
/// [`lookup_host`](crate::net::lookup_host) and
/// [`TcpStream::connect`](crate::net::TcpStream::connect) use one per worker
/// built from the system's files, which it reloads when they change.
///
/// Name Service Switch sources other than files and DNS (mDNS, LDAP, NIS) are
/// not consulted, and names must be ASCII (use punycode for others).
#[derive(Clone)]
pub struct Resolver {
    inner: Rc<Inner>,
}

struct Inner {
    cfg: ResolverConfig,
    cache: RefCell<HashMap<String, (Vec<IpAddr>, Instant)>>,
    /// Round-robin start for `rotate`.
    next_server: std::cell::Cell<usize>,
}

impl fmt::Debug for Resolver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Resolver")
            .field("nameservers", &self.inner.cfg.nameservers)
            .field("search", &self.inner.cfg.search)
            .finish()
    }
}

impl Resolver {
    /// A resolver with this configuration and an empty cache.
    pub fn new(cfg: ResolverConfig) -> Self {
        Self {
            inner: Rc::new(Inner {
                cfg,
                cache: RefCell::new(HashMap::new()),
                next_server: std::cell::Cell::new(0),
            }),
        }
    }

    /// The configuration this resolver uses.
    pub fn config(&self) -> &ResolverConfig {
        &self.inner.cfg
    }

    /// The addresses of `host`: IPv6 and IPv4, in the order the server or the
    /// hosts file gave them. An IP address literal is returned as it is.
    ///
    /// Fails with [`NotFound`](io::ErrorKind::NotFound) if the name does not
    /// exist or has no addresses, [`TimedOut`](io::ErrorKind::TimedOut) if no
    /// server answered, and [`InvalidInput`](io::ErrorKind::InvalidInput) for
    /// a name that is not a valid host name.
    pub async fn lookup_ip(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(vec![ip]);
        }
        validate_name(host)?;
        let cfg = &self.inner.cfg;
        let key = host.trim_end_matches('.').to_ascii_lowercase();
        if key == "localhost" || key.ends_with(".localhost") {
            return Ok(localhost(cfg.hosts_lookup(host)));
        }
        let found = cfg.hosts_lookup(host);
        if !found.is_empty() {
            return Ok(found);
        }
        if let Some(addrs) = self.cached(&key) {
            return Ok(addrs);
        }
        let mut last_err = None;
        for name in cfg.candidates(host) {
            match self.query(&name).await {
                Ok(Outcome::Found(addrs, ttl)) => {
                    self.store(key, addrs.clone(), ttl);
                    return Ok(addrs);
                }
                Ok(Outcome::Missing) => {}
                // A server problem with one candidate: try the next one, but
                // report the problem if no candidate resolves.
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            raise(
                Code::HostNotFound,
                format!("no address found for {host}"),
                "check the spelling of the name, /etc/hosts, and the DNS servers in \
                 /etc/resolv.conf (in a container, the network it is attached to).",
            )
        }))
    }

    fn cached(&self, key: &str) -> Option<Vec<IpAddr>> {
        let mut cache = self.inner.cache.borrow_mut();
        match cache.get(key) {
            Some((addrs, expires)) if *expires > Instant::now() => Some(addrs.clone()),
            Some(_) => {
                cache.remove(key);
                None
            }
            None => None,
        }
    }

    fn store(&self, key: String, addrs: Vec<IpAddr>, ttl: Duration) {
        let ttl = ttl.min(MAX_CACHE_TTL);
        if ttl.is_zero() {
            return;
        }
        let mut cache = self.inner.cache.borrow_mut();
        if cache.len() >= MAX_CACHE_ENTRIES {
            let now = Instant::now();
            cache.retain(|_, (_, expires)| *expires > now);
            if cache.len() >= MAX_CACHE_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(key, (addrs, Instant::now() + ttl));
    }

    /// Asks the servers for `name`, trying each in turn, `attempts` rounds.
    async fn query(&self, name: &str) -> io::Result<Outcome> {
        let cfg = &self.inner.cfg;
        let servers = &cfg.nameservers;
        if servers.is_empty() {
            return Err(raise(
                Code::NoDnsServer,
                "no DNS server is configured",
                "add a nameserver line to /etc/resolv.conf, or use an address or a name from \
                 /etc/hosts.",
            ));
        }
        let start = if cfg.rotate {
            let n = self.inner.next_server.get();
            self.inner.next_server.set(n.wrapping_add(1));
            n % servers.len()
        } else {
            0
        };
        let mut last_err = None;
        for _ in 0..cfg.attempts {
            for k in 0..servers.len() {
                let server = servers[(start + k) % servers.len()];
                match ask_both(server, name, cfg.timeout).await {
                    Ok(outcome) => return Ok(outcome),
                    Err(e) => last_err = Some(e),
                }
            }
        }
        Err(last_err.expect("at least one attempt"))
    }
}

/// What the servers said about one name.
enum Outcome {
    /// Addresses, and how long they may be cached.
    Found(Vec<IpAddr>, Duration),
    /// The name does not exist, or has no address records.
    Missing,
}

/// Sends the `A` and the `AAAA` query to `server` at the same time and
/// waits up to `wait` for each answer. Addresses in one answer are enough:
/// some servers never answer `AAAA` queries, and that must not lose the
/// `A` records.
async fn ask_both(server: SocketAddr, name: &str, wait: Duration) -> io::Result<Outcome> {
    let ask = |qtype| async move {
        match timeout(wait, exchange(server, name, qtype)).await {
            Ok(answer) => answer,
            Err(_) => Err(raise(
                Code::DnsTimeout,
                format!("DNS server {server} did not answer for {name} within {wait:?}"),
                "check that the server in /etc/resolv.conf is reachable (firewall, VPN, \
                 container network); options timeout:N and attempts:N there give it longer.",
            )),
        }
    };
    let (a, aaaa) = join(ask(TYPE_A), ask(TYPE_AAAA)).await;
    let mut addrs = Vec::new();
    let mut ttl = u32::MAX;
    let mut err = None;
    // IPv6 first, as getaddrinfo orders them on a host with IPv6.
    for answer in [aaaa, a] {
        match answer {
            Ok(answer) if answer.rcode == RCODE_NOERROR => {
                if !answer.addrs.is_empty() {
                    ttl = ttl.min(answer.min_ttl);
                    addrs.extend(answer.addrs);
                }
            }
            Ok(answer) if answer.rcode == RCODE_NXDOMAIN => {}
            Ok(answer) => {
                err = Some(raise(
                    Code::DnsServerFailure,
                    format!(
                        "DNS server {server} answered {} for {name}",
                        rcode_name(answer.rcode)
                    ),
                    "the server could not or would not answer; try again, or check the \
                     server's own logs and upstream.",
                ))
            }
            Err(e) => err = Some(e),
        }
    }
    if !addrs.is_empty() {
        return Ok(Outcome::Found(addrs, Duration::from_secs(ttl.into())));
    }
    match err {
        Some(e) => Err(e),
        // NXDOMAIN, or no address records of either type.
        None => Ok(Outcome::Missing),
    }
}

fn rcode_name(rcode: u8) -> String {
    match rcode {
        1 => "FORMERR".into(),
        2 => "SERVFAIL".into(),
        4 => "NOTIMP".into(),
        5 => "REFUSED".into(),
        n => format!("rcode {n}"),
    }
}

/// One query and its answer: over UDP, and again over TCP if the answer was
/// truncated.
async fn exchange(server: SocketAddr, name: &str, qtype: u16) -> io::Result<Answer> {
    let id = random_id();
    let query = build_query(id, name, qtype)?;
    let local: SocketAddr = match server {
        SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
        SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
    };
    // Not `UdpSocket::bind`: its SO_REUSEPORT would let the kernel give two
    // concurrent queries the same port, and their answers could cross.
    let socket = UdpSocket::from_std(std::net::UdpSocket::bind(local)?);
    // Connected: the kernel drops datagrams from anyone but the server.
    socket.connect(server)?;
    let (res, query) = socket.send(query).await;
    res?;
    loop {
        let datagram = socket.recv().await?;
        // Anything that does not match the query (a late answer to an earlier
        // query, garbage) is ignored; the caller's timeout ends the wait.
        let Ok(answer) = parse_response(&datagram, id, name, qtype) else {
            continue;
        };
        drop(datagram);
        if answer.truncated {
            return exchange_tcp(server, &query, id, name, qtype).await;
        }
        return Ok(answer);
    }
}

async fn exchange_tcp(
    server: SocketAddr,
    query: &[u8],
    id: u16,
    name: &str,
    qtype: u16,
) -> io::Result<Answer> {
    let stream = TcpStream::connect_addr(server).await?;
    let mut framed = Vec::with_capacity(query.len() + 2);
    framed.extend_from_slice(&(query.len() as u16).to_be_bytes());
    framed.extend_from_slice(query);
    stream.write_all(framed).await.0?;
    let mut buf = Vec::new();
    loop {
        if buf.len() >= 2 {
            let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
            if buf.len() >= 2 + len {
                return parse_response(&buf[2..2 + len], id, name, qtype);
            }
        }
        let chunk = stream.recv().await?;
        if chunk.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                crate::error::Error::new(
                    Code::DnsBadResponse,
                    format!("DNS server {server} closed the TCP connection before answering"),
                    "check the DNS server; a firewall that cuts TCP on port 53 does this.",
                ),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
}

/// Runs two futures to completion side by side.
async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ra, mut rb) = (None, None);
    poll_fn(|cx| {
        if ra.is_none() {
            if let Poll::Ready(v) = a.as_mut().poll(cx) {
                ra = Some(v);
            }
        }
        if rb.is_none() {
            if let Poll::Ready(v) = b.as_mut().poll(cx) {
                rb = Some(v);
            }
        }
        if ra.is_some() && rb.is_some() {
            Poll::Ready((ra.take().unwrap(), rb.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

fn random_id() -> u16 {
    let mut id = [0u8; 2];
    // SAFETY: getrandom writes at most `id.len()` bytes into `id`.
    let n = unsafe { libc::getrandom(id.as_mut_ptr().cast(), id.len(), 0) };
    if n != 2 {
        // Not cryptographic, but never the same twice in a row.
        let nanos = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        return (nanos ^ (nanos >> 16)) as u16;
    }
    u16::from_ne_bytes(id)
}

fn validate_name(name: &str) -> io::Result<()> {
    let bare = name.strip_suffix('.').unwrap_or(name);
    let valid = !bare.is_empty()
        && bare.len() <= 253
        && bare.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        });
    if valid {
        Ok(())
    } else {
        Err(invalid_host_name(name))
    }
}

// ----------------------------------------------------------- wire format

/// What one DNS response said.
#[derive(Debug)]
struct Answer {
    rcode: u8,
    truncated: bool,
    addrs: Vec<IpAddr>,
    min_ttl: u32,
}

/// A recursive query for `name` (no trailing dot) of type `qtype`.
fn build_query(id: u16, name: &str, qtype: u16) -> io::Result<Vec<u8>> {
    let mut q = Vec::with_capacity(18 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // RD: recursion desired
    q.extend_from_slice(&[0, 1, 0, 0, 0, 0, 0, 0]); // one question
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(invalid_host_name(name));
        }
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.push(0);
    q.extend_from_slice(&qtype.to_be_bytes());
    q.extend_from_slice(&CLASS_IN.to_be_bytes());
    Ok(q)
}

fn invalid_host_name(name: &str) -> io::Error {
    raise(
        Code::InvalidHostName,
        format!("not a valid host name: {name:?}"),
        "a host name is letters, digits, '-' and '_' in dot-separated labels of 1 to 63 \
         characters, 253 characters in all.",
    )
}

fn malformed() -> io::Error {
    raise(
        Code::DnsBadResponse,
        "malformed DNS response",
        "check the DNS server in /etc/resolv.conf; something between giong and it may be \
         changing the packets.",
    )
}

fn read_u16(buf: &[u8], pos: usize) -> io::Result<u16> {
    buf.get(pos..pos + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or_else(malformed)
}

fn read_u32(buf: &[u8], pos: usize) -> io::Result<u32> {
    buf.get(pos..pos + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or_else(malformed)
}

/// Reads a possibly compressed name at `pos`. Returns it in lower case,
/// without a trailing dot, and the position after it in the message.
fn read_name(buf: &[u8], mut pos: usize) -> io::Result<(String, usize)> {
    let mut name = String::new();
    let mut end = None;
    let mut jumps = 0;
    loop {
        let len = *buf.get(pos).ok_or_else(malformed)? as usize;
        match len & 0xC0 {
            0x00 if len == 0 => {
                return Ok((name, end.unwrap_or(pos + 1)));
            }
            0x00 => {
                let label = buf.get(pos + 1..pos + 1 + len).ok_or_else(malformed)?;
                if !name.is_empty() {
                    name.push('.');
                }
                name.extend(label.iter().map(|b| b.to_ascii_lowercase() as char));
                if name.len() > 255 {
                    return Err(malformed());
                }
                pos += 1 + len;
            }
            0xC0 => {
                let target = (read_u16(buf, pos)? & 0x3FFF) as usize;
                end.get_or_insert(pos + 2);
                jumps += 1;
                if jumps > 64 {
                    return Err(malformed());
                }
                pos = target;
            }
            _ => return Err(malformed()),
        }
    }
}

/// Parses the response to query `id` for `name` and `qtype`, following CNAME
/// records inside the response.
fn parse_response(buf: &[u8], id: u16, name: &str, qtype: u16) -> io::Result<Answer> {
    if buf.len() < 12 || read_u16(buf, 0)? != id {
        return Err(malformed());
    }
    let flags = read_u16(buf, 2)?;
    let is_response = flags & 0x8000 != 0;
    let opcode = (flags >> 11) & 0xF;
    if !is_response || opcode != 0 {
        return Err(malformed());
    }
    let truncated = flags & 0x0200 != 0;
    let rcode = (flags & 0xF) as u8;
    let qdcount = read_u16(buf, 4)?;
    let ancount = read_u16(buf, 6)?;
    if qdcount != 1 {
        return Err(malformed());
    }
    let (qname, mut pos) = read_name(buf, 12)?;
    let wanted = name.trim_end_matches('.').to_ascii_lowercase();
    if qname != wanted || read_u16(buf, pos)? != qtype || read_u16(buf, pos + 2)? != CLASS_IN {
        return Err(malformed());
    }
    pos += 4;

    let mut records = Vec::with_capacity(ancount as usize);
    if !truncated {
        for _ in 0..ancount {
            let (owner, next) = read_name(buf, pos)?;
            let rtype = read_u16(buf, next)?;
            let class = read_u16(buf, next + 2)?;
            let ttl = read_u32(buf, next + 4)?;
            let rdlen = read_u16(buf, next + 8)? as usize;
            let rdata = next + 10;
            if buf.len() < rdata + rdlen {
                return Err(malformed());
            }
            if class == CLASS_IN {
                records.push((owner, rtype, ttl, rdata, rdlen));
            }
            pos = rdata + rdlen;
        }
    }

    // The query name and every alias it leads to through CNAME records.
    let mut names = vec![wanted];
    let mut min_ttl = u32::MAX;
    loop {
        let mut grew = false;
        for (owner, rtype, ttl, rdata, _) in &records {
            if *rtype == TYPE_CNAME && names.contains(owner) {
                let (target, _) = read_name(buf, *rdata)?;
                if !names.contains(&target) {
                    names.push(target);
                    min_ttl = min_ttl.min(*ttl);
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    let mut addrs = Vec::new();
    for (owner, rtype, ttl, rdata, rdlen) in &records {
        if *rtype != qtype || !names.contains(owner) {
            continue;
        }
        let data = &buf[*rdata..*rdata + *rdlen];
        let ip: IpAddr = match (qtype, data.len()) {
            (TYPE_A, 4) => Ipv4Addr::new(data[0], data[1], data[2], data[3]).into(),
            (TYPE_AAAA, 16) => {
                let mut octets = [0u8; 16];
                octets.copy_from_slice(data);
                Ipv6Addr::from(octets).into()
            }
            _ => return Err(malformed()),
        };
        min_ttl = min_ttl.min(*ttl);
        if !addrs.contains(&ip) {
            addrs.push(ip);
        }
    }
    Ok(Answer {
        rcode,
        truncated,
        addrs,
        min_ttl,
    })
}

// --------------------------------------------------------- system resolver

thread_local! {
    /// The worker's resolver built from the system files, and when the files
    /// were last checked.
    static SYSTEM: RefCell<Option<SystemResolver>> = const { RefCell::new(None) };
}

struct SystemResolver {
    resolver: Resolver,
    checked: Instant,
    stamps: [Option<SystemTime>; 2],
}

fn file_stamps() -> [Option<SystemTime>; 2] {
    ["/etc/resolv.conf", "/etc/hosts"].map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

/// The current thread's resolver for the system configuration, reloaded when
/// `/etc/resolv.conf` or `/etc/hosts` change (checked every few seconds).
pub(crate) fn system_resolver() -> io::Result<Resolver> {
    SYSTEM.with(|slot| {
        let mut slot = slot.borrow_mut();
        let now = Instant::now();
        if let Some(sys) = slot.as_mut() {
            if now.duration_since(sys.checked) < RELOAD_CHECK {
                return Ok(sys.resolver.clone());
            }
            sys.checked = now;
            let stamps = file_stamps();
            if stamps == sys.stamps {
                return Ok(sys.resolver.clone());
            }
        }
        let stamps = file_stamps();
        let resolver = Resolver::new(ResolverConfig::system()?);
        *slot = Some(SystemResolver {
            resolver: resolver.clone(),
            checked: now,
            stamps,
        });
        Ok(resolver)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response to `query` with the given flags and answer records.
    fn response(
        query: &[u8],
        rcode: u8,
        truncated: bool,
        answers: &[(&[u8], u16, u32, &[u8])],
    ) -> Vec<u8> {
        let mut r = query.to_vec();
        let mut flags = 0x8180u16 | rcode as u16;
        if truncated {
            flags |= 0x0200;
        }
        r[2..4].copy_from_slice(&flags.to_be_bytes());
        r[6..8].copy_from_slice(&(answers.len() as u16).to_be_bytes());
        for (owner, rtype, ttl, rdata) in answers {
            r.extend_from_slice(owner);
            r.extend_from_slice(&rtype.to_be_bytes());
            r.extend_from_slice(&CLASS_IN.to_be_bytes());
            r.extend_from_slice(&ttl.to_be_bytes());
            r.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            r.extend_from_slice(rdata);
        }
        r
    }

    #[test]
    fn queries_are_encoded_as_the_rfc_says() {
        let q = build_query(0xABCD, "api.example.com", TYPE_AAAA).unwrap();
        assert_eq!(
            q,
            b"\xAB\xCD\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00\
              \x03api\x07example\x03com\x00\x00\x1C\x00\x01"
        );
        assert!(build_query(1, "a..b", TYPE_A).is_err());
        assert!(build_query(1, &"x".repeat(64), TYPE_A).is_err());
    }

    #[test]
    fn answers_follow_cnames_and_compression() {
        let q = build_query(7, "www.example.com", TYPE_A).unwrap();
        // www.example.com CNAME edge.example.net (written with a pointer to
        // the question name for the owner), then the A records of the target.
        let edge = b"\x04edge\x07example\x03net\x00";
        let r = response(
            &q,
            0,
            false,
            &[
                (b"\xC0\x0C", TYPE_CNAME, 300, edge),
                (
                    b"\x04edge\x07example\x03net\x00",
                    TYPE_A,
                    60,
                    &[192, 0, 2, 1],
                ),
                (
                    b"\x04edge\x07example\x03net\x00",
                    TYPE_A,
                    120,
                    &[192, 0, 2, 2],
                ),
                // Not on the chain: ignored.
                (b"\x05other\x00", TYPE_A, 1, &[10, 0, 0, 1]),
            ],
        );
        let a = parse_response(&r, 7, "WWW.Example.com.", TYPE_A).unwrap();
        assert_eq!(a.rcode, 0);
        assert_eq!(
            a.addrs,
            [IpAddr::from([192, 0, 2, 1]), IpAddr::from([192, 0, 2, 2])]
        );
        assert_eq!(a.min_ttl, 60);
    }

    #[test]
    fn mismatched_or_broken_responses_are_rejected() {
        let q = build_query(9, "example.com", TYPE_A).unwrap();
        let r = response(&q, 0, false, &[(b"\xC0\x0C", TYPE_A, 5, &[1, 2, 3, 4])]);
        assert!(
            parse_response(&r, 10, "example.com", TYPE_A).is_err(),
            "wrong id"
        );
        assert!(
            parse_response(&r, 9, "example.org", TYPE_A).is_err(),
            "wrong name"
        );
        assert!(
            parse_response(&r, 9, "example.com", TYPE_AAAA).is_err(),
            "wrong type"
        );
        assert!(
            parse_response(&q, 9, "example.com", TYPE_A).is_err(),
            "a query"
        );
        for cut in 12..r.len() {
            let _ = parse_response(&r[..cut], 9, "example.com", TYPE_A);
        }
        // A pointer loop ends in an error, not a hang.
        let mut looped = r.clone();
        let at = q.len();
        looped[at..at + 2].copy_from_slice(&[0xC0, at as u8]);
        assert!(parse_response(&looped, 9, "example.com", TYPE_A).is_err());
        // Wrong address length.
        let bad = response(&q, 0, false, &[(b"\xC0\x0C", TYPE_A, 5, &[1, 2, 3])]);
        assert!(parse_response(&bad, 9, "example.com", TYPE_A).is_err());
    }

    #[test]
    fn rcodes_and_truncation_are_reported() {
        let q = build_query(3, "gone.example", TYPE_A).unwrap();
        let nx = parse_response(&response(&q, 3, false, &[]), 3, "gone.example", TYPE_A).unwrap();
        assert_eq!((nx.rcode, nx.addrs.len()), (RCODE_NXDOMAIN, 0));
        let tc = parse_response(&response(&q, 0, true, &[]), 3, "gone.example", TYPE_A).unwrap();
        assert!(tc.truncated);
    }

    #[test]
    fn resolv_conf_is_read_like_glibc_reads_it() {
        let cfg = ResolverConfig::parse(
            "# comment\n\
             nameserver 10.96.0.10\n\
             nameserver 2001:db8::53 ; trailing comment\n\
             nameserver fe80::1%eth0\n\
             nameserver 10.0.0.3\nnameserver 10.0.0.4\n\
             domain ignored.example\n\
             search default.svc.cluster.local svc.cluster.local cluster.local.\n\
             options ndots:5 timeout:2 attempts:3 rotate edns0\n",
            "127.0.0.1 localhost\n::1 localhost ip6-localhost # loopback\n10.1.2.3 db DB.internal.\n",
        );
        assert_eq!(
            cfg.nameservers,
            [
                "10.96.0.10:53".parse().unwrap(),
                "[2001:db8::53]:53".parse().unwrap(),
                "10.0.0.3:53".parse().unwrap(),
            ]
        );
        assert_eq!(
            cfg.search,
            [
                "default.svc.cluster.local",
                "svc.cluster.local",
                "cluster.local"
            ]
        );
        assert_eq!(
            (cfg.ndots, cfg.timeout, cfg.attempts, cfg.rotate),
            (5, Duration::from_secs(2), 3, true)
        );
        assert_eq!(
            cfg.hosts_lookup("LOCALHOST"),
            [
                IpAddr::from([127, 0, 0, 1]),
                "::1".parse::<IpAddr>().unwrap()
            ]
        );
        assert_eq!(
            cfg.hosts_lookup("db.internal."),
            [IpAddr::from([10, 1, 2, 3])]
        );

        let empty = ResolverConfig::parse("", "");
        assert_eq!(empty.nameservers, ["127.0.0.1:53".parse().unwrap()]);
        assert_eq!((empty.ndots, empty.attempts), (1, 2));
    }

    #[test]
    fn the_search_list_follows_ndots() {
        let cfg = ResolverConfig::parse("search a.example b.example\noptions ndots:2\n", "");
        assert_eq!(cfg.candidates("db"), ["db.a.example", "db.b.example", "db"]);
        assert_eq!(
            cfg.candidates("db.x"),
            ["db.x.a.example", "db.x.b.example", "db.x"]
        );
        assert_eq!(
            cfg.candidates("db.x.y"),
            ["db.x.y", "db.x.y.a.example", "db.x.y.b.example"]
        );
        assert_eq!(cfg.candidates("db.x."), ["db.x"]);
    }

    #[test]
    fn query_ids_are_random() {
        let ids: std::collections::HashSet<u16> = (0..64).map(|_| random_id()).collect();
        // 64 draws from 65,536 values: a repeat or two at most.
        assert!(ids.len() > 60, "{} distinct ids", ids.len());
    }

    #[test]
    fn host_names_are_validated() {
        for ok in [
            "localhost",
            "a-b.example.com.",
            "_srv.example",
            &"a".repeat(63),
        ] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", ".", "a..b", "-", "bad name", "ví.dụ", &"a".repeat(64)] {
            if bad == "-" {
                continue;
            }
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }
}
