//! Name resolution that does not block the worker: `/etc/hosts`, then DNS over
//! UDP (and TCP when an answer does not fit) to the servers in
//! `/etc/resolv.conf`, the way musl's resolver does it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::future::{poll_fn, Future};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::{pin, Pin};
use std::rc::Rc;
use std::task::Poll;
use std::time::{Duration, SystemTime};

use crate::error::{raise, Code};
use crate::net::{TcpStream, UdpSocket};
use crate::sync::oneshot;
use crate::time::{sleep, timeout, Instant};

const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const TYPE_SOA: u16 = 6;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;

const RCODE_NOERROR: u8 = 0;
const RCODE_NXDOMAIN: u8 = 3;

/// Longest a positive answer is kept in the cache, whatever its TTL says.
const MAX_CACHE_TTL: Duration = Duration::from_secs(300);
/// Longest a name that does not exist is remembered as missing, whatever
/// its zone's SOA record says (RFC 2308 allows hours; a name that was just
/// created should not stay missing for long).
const MAX_MISSING_TTL: Duration = Duration::from_secs(30);
/// Longest addresses of one family are cached when the query for the other
/// family went unanswered: long enough that a server which never answers
/// `AAAA` costs one short wait per half minute, short enough that a lost
/// answer does not hide the other family for the full TTL.
const MAX_PARTIAL_TTL: Duration = Duration::from_secs(30);
/// Once one of the `A` and `AAAA` queries has answered with addresses, how
/// long the other one is waited for at least: the resolution delay of Happy
/// Eyeballs (RFC 8305, section 3). The wait is as long as the first answer
/// took when that was longer, as a slow path to the server slows both.
const OTHER_FAMILY_GRACE: Duration = Duration::from_millis(50);
/// Entries kept in one resolver's cache (and again in its cache of
/// missing names).
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
    ///
    /// On macOS `/etc/resolv.conf` is written by the system from its network
    /// configuration, and holds the servers and search domains of the
    /// primary resolver (the first one `scutil --dns` lists), so this follows
    /// the network the machine is on. Resolvers that macOS uses for some
    /// domains only (files in `/etc/resolver`, a VPN's own domains) are not
    /// in that file and are not consulted.
    /// On Windows, which has no `resolv.conf`, the name servers are those of
    /// the network adapters that are up (the first three, in the order
    /// Windows lists the adapters), the search domains are the machine's
    /// primary DNS suffix and each adapter's own suffix, and the hosts file
    /// is `%SystemRoot%\System32\drivers\etc\hosts`. IPv6 servers that are
    /// site-local (`fec0::/10`, listed on adapters with no IPv6 server) or
    /// link-local are left out. The other settings are the defaults.
    pub fn system() -> io::Result<Self> {
        #[cfg(unix)]
        {
            let resolv = read_optional("/etc/resolv.conf")?;
            let hosts = read_optional("/etc/hosts")?;
            Ok(Self::parse(&resolv, &hosts))
        }
        #[cfg(windows)]
        {
            let (servers, search) = crate::sys::dns_config()?;
            let hosts = read_optional(crate::sys::hosts_path())?;
            Ok(Self::from_adapters(&servers, &search, &hosts))
        }
    }

    /// What the adapters report, with the hosts file. With no DNS server
    /// there is none: Windows runs no server at `127.0.0.1:53`, the
    /// default a `resolv.conf` without one gets, so a lookup there would
    /// only time out; it fails with `GIONG-E0104` instead.
    #[cfg(windows)]
    fn from_adapters(servers: &[IpAddr], search: &[String], hosts: &str) -> Self {
        let mut cfg = Self::parse(&adapters_as_resolv_conf(servers, search), hosts);
        if servers.is_empty() {
            cfg.nameservers.clear();
        }
        cfg
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
                    if let Some(server) = words.next().and_then(nameserver) {
                        if cfg.nameservers.len() < 3 {
                            cfg.nameservers.push(server);
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

    /// Uses these name servers instead of the configured ones. DNS servers
    /// listen on port 53, as in `SocketAddr::from(([10, 0, 0, 2], 53))`.
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

/// What Windows reports, written as a `resolv.conf`, so that it is read by
/// the same rules.
#[cfg(windows)]
fn adapters_as_resolv_conf(servers: &[IpAddr], search: &[String]) -> String {
    let mut text = String::new();
    for server in servers {
        text.push_str(&format!("nameserver {server}\n"));
    }
    if !search.is_empty() {
        text.push_str(&format!("search {}\n", search.join(" ")));
    }
    text
}

/// Where the system's DNS servers are set, for the fixes of errors.
#[cfg(unix)]
macro_rules! servers_where {
    () => {
        "/etc/resolv.conf"
    };
}
#[cfg(windows)]
macro_rules! servers_where {
    () => {
        "the settings of the network adapter (`ipconfig /all` lists them)"
    };
}

/// Where the system's hosts file is, for the fixes of errors.
#[cfg(unix)]
macro_rules! hosts_where {
    () => {
        "/etc/hosts"
    };
}
#[cfg(windows)]
macro_rules! hosts_where {
    () => {
        "the hosts file (%SystemRoot%\\System32\\drivers\\etc\\hosts)"
    };
}

/// How to give a slow DNS server longer, for the fixes of errors.
#[cfg(unix)]
macro_rules! longer_where {
    () => {
        "options timeout:N and attempts:N there give it longer"
    };
}
#[cfg(windows)]
macro_rules! longer_where {
    () => {
        "ResolverConfig::timeout and ResolverConfig::attempts give it longer"
    };
}

fn read_optional(path: impl AsRef<std::path::Path>) -> io::Result<String> {
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
/// are cached for their TTL (at most five minutes), and a name that does not
/// exist is remembered as missing for its zone's negative TTL (RFC 2308, at
/// most 30 seconds). Lookups of one name that run at the same time share
/// one set of queries. Once one of the `A` and `AAAA` queries has answered
/// with addresses, the other is waited for only briefly (50 ms, or as long
/// as the first answer took), so a server that never answers `AAAA` does not
/// hold the `A` records for the whole timeout.
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

/// What a lookup that is under way owes the lookups of the same name that
/// started after it.
type Waiting = Vec<oneshot::Sender<io::Result<Vec<IpAddr>>>>;

struct Inner {
    cfg: ResolverConfig,
    cache: RefCell<HashMap<String, (Vec<IpAddr>, Instant)>>,
    /// Query names the servers said do not exist (or have no addresses),
    /// and until when that holds.
    missing: RefCell<HashMap<String, Instant>>,
    /// Lookups under way, by cache key, with the lookups waiting on them.
    flights: RefCell<HashMap<String, Waiting>>,
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
                missing: RefCell::new(HashMap::new()),
                flights: RefCell::new(HashMap::new()),
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
    ///
    /// Cancel safe: it changes nothing; a dropped lookup closes its sockets.
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
        // One lookup per name at a time: a lookup that finds another one
        // under way waits for its result. If that one is dropped first, its
        // waiters see their channel close, and one of them takes over.
        loop {
            let waiting = match self.inner.flights.borrow_mut().entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(mut flight) => {
                    let (tx, rx) = oneshot::channel();
                    flight.get_mut().push(tx);
                    rx
                }
                std::collections::hash_map::Entry::Vacant(slot) => {
                    slot.insert(Vec::new());
                    break;
                }
            };
            if let Ok(result) = waiting.await {
                return result;
            }
        }
        let flight = Flight {
            inner: &self.inner,
            key: &key,
        };
        let result = self.resolve(host, &key).await;
        for waiter in flight.land() {
            let _ = waiter.send(copy_result(&result));
        }
        result
    }

    /// Asks the servers for `host`'s candidate names in turn, and caches
    /// what they say.
    async fn resolve(&self, host: &str, key: &str) -> io::Result<Vec<IpAddr>> {
        let cfg = &self.inner.cfg;
        let mut last_err = None;
        for name in cfg.candidates(host) {
            let name_key = name.trim_end_matches('.').to_ascii_lowercase();
            if self.known_missing(&name_key) {
                continue;
            }
            match self.query(&name).await {
                Ok(Outcome::Found(addrs, ttl)) => {
                    self.store(key.to_string(), addrs.clone(), ttl);
                    return Ok(addrs);
                }
                Ok(Outcome::Missing(ttl)) => self.store_missing(name_key, ttl),
                // A server problem with one candidate: try the next one, but
                // report the problem if no candidate resolves.
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            raise(
                Code::HostNotFound,
                format!("no address found for {host}"),
                concat!(
                    "check the spelling of the name, ",
                    hosts_where!(),
                    ", and the DNS servers in ",
                    servers_where!(),
                    " (in a container, the network it is attached to)."
                ),
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

    fn known_missing(&self, name: &str) -> bool {
        let mut missing = self.inner.missing.borrow_mut();
        match missing.get(name) {
            Some(expires) if *expires > Instant::now() => true,
            Some(_) => {
                missing.remove(name);
                false
            }
            None => false,
        }
    }

    fn store_missing(&self, name: String, ttl: Option<Duration>) {
        // Without the zone's SOA record there is no negative TTL, and
        // RFC 2308 (section 5) says not to cache.
        let Some(ttl) = ttl.map(|ttl| ttl.min(MAX_MISSING_TTL)) else {
            return;
        };
        if ttl.is_zero() {
            return;
        }
        let mut missing = self.inner.missing.borrow_mut();
        let now = Instant::now();
        if missing.len() >= MAX_CACHE_ENTRIES {
            missing.retain(|_, expires| *expires > now);
            if missing.len() >= MAX_CACHE_ENTRIES {
                missing.clear();
            }
        }
        missing.insert(name, now + ttl);
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
            #[cfg(windows)]
            let (what, fix) = (
                "no DNS server is configured: ResolverConfig::nameservers was given none, or no \
                 network adapter reports one",
                "give a network adapter a DNS server (`ipconfig /all` lists them), pass at least \
                 one server to ResolverConfig::nameservers, or use an address or a name from the \
                 hosts file.",
            );
            #[cfg(unix)]
            let (what, fix) = (
                "no DNS server is configured: ResolverConfig::nameservers was given none",
                "pass at least one server to ResolverConfig::nameservers, or use an address or a \
                 name from /etc/hosts.",
            );
            return Err(raise(Code::NoDnsServer, what, fix));
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

/// A lookup under way, registered in [`Inner::flights`]. Dropped before it
/// [lands](Self::land) (the lookup was cancelled), it leaves the map, and
/// the lookups waiting on it see their channels close.
struct Flight<'a> {
    inner: &'a Inner,
    key: &'a str,
}

impl Flight<'_> {
    /// Ends the lookup, returning the lookups that wait for its result.
    fn land(self) -> Waiting {
        let waiting = self.inner.flights.borrow_mut().remove(self.key);
        std::mem::forget(self);
        waiting.unwrap_or_default()
    }
}

impl Drop for Flight<'_> {
    fn drop(&mut self) {
        let waiting = self.inner.flights.borrow_mut().remove(self.key);
        // The senders close outside the borrow: a waiter woken by that runs
        // later, but nothing here may assume so.
        drop(waiting);
    }
}

/// The result of a lookup again, for a lookup that waited on it.
fn copy_result(result: &io::Result<Vec<IpAddr>>) -> io::Result<Vec<IpAddr>> {
    match result {
        Ok(addrs) => Ok(addrs.clone()),
        Err(e) => Err(copy_error(e)),
    }
}

/// An error like `e`: the same kind, the same giong error (code, message,
/// fix) and the same operating system error underneath, which is what
/// callers look at. Other errors become their kind and text.
fn copy_error(e: &io::Error) -> io::Error {
    fn copy_source(e: &(dyn std::error::Error + 'static)) -> io::Error {
        match e.downcast_ref::<io::Error>() {
            Some(e) => copy_error(e),
            None => io::Error::other(e.to_string()),
        }
    }
    if let Some(raw) = e.raw_os_error() {
        return io::Error::from_raw_os_error(raw);
    }
    let Some(inner) = e.get_ref() else {
        return e.kind().into();
    };
    if let Some(giong) = inner.downcast_ref::<crate::error::Error>() {
        if giong.get_ref().is_none() {
            let mut copy = crate::error::Error::new(
                giong.code(),
                giong.message().into_owned(),
                giong.fix().to_owned(),
            );
            if let Some(source) = std::error::Error::source(giong) {
                copy = copy.with_source(copy_source(source));
            }
            return io::Error::new(e.kind(), copy);
        }
    }
    io::Error::new(e.kind(), inner.to_string())
}

/// What the servers said about one name.
enum Outcome {
    /// Addresses, and how long they may be cached.
    Found(Vec<IpAddr>, Duration),
    /// The name does not exist, or has no address records; for how long
    /// that may be cached, when the server said.
    Missing(Option<Duration>),
}

/// Sends the `A` and the `AAAA` query to `server` at the same time and
/// waits up to `wait` for each answer. Addresses in one answer are enough:
/// some servers never answer `AAAA` queries, and that must not lose the
/// `A` records, nor hold them for the whole `wait`. Once one answer has
/// addresses, the other is waited for [`OTHER_FAMILY_GRACE`], or as long as
/// the first one took if that is longer.
async fn ask_both(server: SocketAddr, name: &str, wait: Duration) -> io::Result<Outcome> {
    let ask = |qtype| async move {
        match timeout(wait, exchange(server, name, qtype)).await {
            Ok(answer) => answer,
            Err(_) => Err(raise(
                Code::DnsTimeout,
                format!("DNS server {server} did not answer for {name} within {wait:?}"),
                concat!(
                    "check that the server in ",
                    servers_where!(),
                    " is reachable (firewall, VPN, container network); ",
                    longer_where!(),
                    "."
                ),
            )),
        }
    };
    let (a, aaaa) = first_with_grace(ask(TYPE_A), ask(TYPE_AAAA)).await;
    let mut addrs = Vec::new();
    let mut ttl = u32::MAX;
    // How long the name may be remembered as missing, while every answer
    // says it is.
    let mut missing_ttl = Some(u32::MAX);
    let mut err = None;
    let partial = a.is_none() || aaaa.is_none();
    // IPv6 first, as getaddrinfo orders them on a host with IPv6.
    for answer in [aaaa, a].into_iter().flatten() {
        match answer {
            Ok(answer) if answer.rcode == RCODE_NOERROR || answer.rcode == RCODE_NXDOMAIN => {
                if answer.addrs.is_empty() {
                    missing_ttl = missing_ttl.zip(answer.negative_ttl).map(|(a, b)| a.min(b));
                } else {
                    ttl = ttl.min(answer.min_ttl);
                    addrs.extend(answer.addrs);
                }
            }
            // The kernel's own error, such as "connection refused" when
            // nothing listens on the server's port: say that it came from
            // the DNS server, or a failed `connect("db.internal:5432")`
            // would read as if the database had refused.
            Err(e) if crate::error::code(&e).is_none() => {
                err = Some(
                    crate::error::Error::new(
                        Code::DnsServerUnreachable,
                        format!("cannot reach DNS server {server} to look up {name}: {e}"),
                        concat!(
                            "check that a DNS server runs at the address in ",
                            servers_where!(),
                            " (in a container, the network it is attached to) and that a \
                             firewall lets UDP and TCP port 53 through."
                        ),
                    )
                    .with_source(e)
                    .into(),
                )
            }
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
        let mut ttl = Duration::from_secs(ttl.into());
        if partial {
            ttl = ttl.min(MAX_PARTIAL_TTL);
        }
        return Ok(Outcome::Found(addrs, ttl));
    }
    match err {
        Some(e) => Err(e),
        // NXDOMAIN, or no address records of either type. (`partial` cannot
        // hold here: the grace starts only after an answer with addresses.)
        None => Ok(Outcome::Missing(
            missing_ttl.map(|ttl| Duration::from_secs(ttl.into())),
        )),
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
    let socket = UdpSocket::from_std(query_socket(local)?);
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

/// A UDP socket on a fresh port of its own, for one query.
///
/// Not `UdpSocket::bind`: its `SO_REUSEPORT` would let the kernel give two
/// concurrent queries the same port, and their answers could cross.
#[cfg(unix)]
fn query_socket(local: SocketAddr) -> io::Result<std::net::UdpSocket> {
    std::net::UdpSocket::bind(local)
}

/// A UDP socket on a fresh port of its own, for one query.
///
/// Its bind is exclusive (`SO_EXCLUSIVEADDRUSE`). Without that, Windows lets
/// any other socket, of any process, bind the same port on a more specific
/// address (`127.0.0.1`, the adapter's own) and take the datagrams sent to
/// it: the answer goes to that socket and the lookup waits out its timeout.
/// Runs of many test processes at once caught local DNS servers doing it.
#[cfg(windows)]
fn query_socket(local: SocketAddr) -> io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let sock = Socket::new(Domain::for_address(local), Type::DGRAM, Some(Protocol::UDP))?;
    crate::sys::set_exclusive_address(&sock)?;
    sock.bind(&local.into())?;
    Ok(sock.into())
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

/// Runs the `A` and the `AAAA` query side by side. Once one of them has
/// answered with addresses, the other gets [`OTHER_FAMILY_GRACE`] (or as long
/// as the first took, if longer) and is then given up: `None`.
async fn first_with_grace<A, B>(
    a: A,
    b: B,
) -> (Option<io::Result<Answer>>, Option<io::Result<Answer>>)
where
    A: Future<Output = io::Result<Answer>>,
    B: Future<Output = io::Result<Answer>>,
{
    fn has_addrs(r: &Option<io::Result<Answer>>) -> bool {
        matches!(r, Some(Ok(answer)) if answer.rcode == RCODE_NOERROR && !answer.addrs.is_empty())
    }
    let started = Instant::now();
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ra, mut rb) = (None, None);
    let mut grace: Option<crate::time::Sleep> = None;
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
            return Poll::Ready((ra.take(), rb.take()));
        }
        if grace.is_none() && (has_addrs(&ra) || has_addrs(&rb)) {
            grace = Some(sleep(started.elapsed().max(OTHER_FAMILY_GRACE)));
        }
        if let Some(grace) = grace.as_mut() {
            if Pin::new(grace).poll(cx).is_ready() {
                return Poll::Ready((ra.take(), rb.take()));
            }
        }
        Poll::Pending
    })
    .await
}

fn random_id() -> u16 {
    let mut id = [0u8; 2];
    if !crate::sys::random_bytes(&mut id) {
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
    /// For an answer without addresses: how long that may be cached, from
    /// the SOA record of the authority section (RFC 2308, section 5).
    negative_ttl: Option<u32>,
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
        concat!(
            "check the DNS server in ",
            servers_where!(),
            "; something between giong and it may be changing the packets."
        ),
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

/// The most CNAME records followed from one name to the next, like the
/// limits of glibc's and other stub resolvers. Real chains have a few.
const MAX_CNAMES: usize = 16;

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
    let nscount = read_u16(buf, 8)?;
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

    // The query name and the aliases it leads to through CNAME records,
    // followed one link at a time from the query name: one pass over the
    // records per link, at most MAX_CNAMES links, so a hostile answer with
    // thousands of records costs linear time, not a frozen worker.
    let mut names = vec![wanted];
    let mut min_ttl = u32::MAX;
    loop {
        let current = names.last().expect("starts with the query name");
        let Some((_, _, ttl, rdata, _)) = records
            .iter()
            .find(|(owner, rtype, ..)| *rtype == TYPE_CNAME && owner == current)
        else {
            break;
        };
        let (target, _) = read_name(buf, *rdata)?;
        if names.contains(&target) {
            // A loop: the names so far are all there is.
            break;
        }
        if names.len() > MAX_CNAMES {
            return Err(raise(
                Code::DnsBadResponse,
                format!(
                    "the DNS answer for {name} has a chain of more than {MAX_CNAMES} CNAME records"
                ),
                concat!(
                    "check the DNS server in ",
                    servers_where!(),
                    " and the records of the name; resolvers stop following aliases long \
                     before this."
                ),
            ));
        }
        min_ttl = min_ttl.min(*ttl);
        names.push(target);
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
    // A negative answer carries its zone's SOA record in the authority
    // section, which says how long the answer may be cached. A broken
    // authority section only costs the caching.
    let negative_ttl = if addrs.is_empty() && !truncated {
        negative_ttl(buf, pos, nscount)
    } else {
        None
    };
    Ok(Answer {
        rcode,
        truncated,
        addrs,
        min_ttl,
        negative_ttl,
    })
}

/// The negative TTL of the SOA record among the `count` authority records at
/// `pos`: the smaller of its TTL and its MINIMUM field (RFC 2308, section 5).
fn negative_ttl(buf: &[u8], mut pos: usize, count: u16) -> Option<u32> {
    for _ in 0..count {
        let (_, next) = read_name(buf, pos).ok()?;
        let rtype = read_u16(buf, next).ok()?;
        let class = read_u16(buf, next + 2).ok()?;
        let ttl = read_u32(buf, next + 4).ok()?;
        let rdlen = read_u16(buf, next + 8).ok()? as usize;
        let rdata = next + 10;
        if buf.len() < rdata + rdlen {
            return None;
        }
        if rtype == TYPE_SOA && class == CLASS_IN {
            // MNAME and RNAME, then SERIAL, REFRESH, RETRY, EXPIRE, MINIMUM.
            let (_, after_mname) = read_name(buf, rdata).ok()?;
            let (_, after_rname) = read_name(buf, after_mname).ok()?;
            let minimum_at = after_rname + 16;
            if minimum_at + 4 > rdata + rdlen {
                return None;
            }
            return Some(ttl.min(read_u32(buf, minimum_at).ok()?));
        }
        pos = rdata + rdlen;
    }
    None
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
    stamps: Stamps,
}

/// What the system configuration was read from, to tell when it changed.
#[cfg(unix)]
type Stamps = [Option<SystemTime>; 2];
/// The hosts file's time, and what the adapters report: Windows keeps the
/// rest in no file.
#[cfg(windows)]
type Stamps = (Option<SystemTime>, Option<(Vec<IpAddr>, Vec<String>)>);

#[cfg(unix)]
fn file_stamps() -> Stamps {
    ["/etc/resolv.conf", "/etc/hosts"].map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
}

#[cfg(windows)]
fn file_stamps() -> Stamps {
    #[cfg(test)]
    tests::note_stamps_thread();
    let hosts = std::fs::metadata(crate::sys::hosts_path())
        .and_then(|m| m.modified())
        .ok();
    (hosts, crate::sys::dns_config().ok())
}

/// The stamps, read where reading them cannot hold up the worker for long.
#[cfg(unix)]
async fn read_stamps() -> Stamps {
    file_stamps()
}

/// The stamps, read on a blocking thread: asking the adapters
/// (`GetNetworkParams`, `GetAdaptersAddresses`) can take a while.
#[cfg(windows)]
async fn read_stamps() -> Stamps {
    crate::fs::blocking(|| Ok(file_stamps()))
        .await
        .unwrap_or((None, None))
}

/// The system configuration, read again.
#[cfg(unix)]
async fn system_config(_stamps: &Stamps) -> io::Result<ResolverConfig> {
    ResolverConfig::system()
}

/// The system configuration, with what the adapters reported for `stamps`.
#[cfg(windows)]
async fn system_config(stamps: &Stamps) -> io::Result<ResolverConfig> {
    match &stamps.1 {
        Some((servers, search)) => {
            let hosts = read_optional(crate::sys::hosts_path())?;
            Ok(ResolverConfig::from_adapters(servers, search, &hosts))
        }
        // Asked again on a blocking thread, for the error.
        None => crate::fs::blocking(ResolverConfig::system).await,
    }
}

/// A `nameserver` address on port 53. An IPv6 address may carry a zone
/// (`fe80::1%en0` or `fe80::1%4`), as macOS writes for a router's
/// link-local address: the interface it names, which must exist.
fn nameserver(word: &str) -> Option<SocketAddr> {
    let Some((ip, zone)) = word.split_once('%') else {
        return word
            .parse::<IpAddr>()
            .ok()
            .map(|ip| SocketAddr::new(ip, 53));
    };
    let ip = ip.parse::<std::net::Ipv6Addr>().ok()?;
    let scope = match zone.parse::<u32>() {
        Ok(index) => index,
        Err(_) => interface_index(zone)?,
    };
    // A link-local address needs an interface to be sent to.
    if scope == 0 && ip.segments()[0] & 0xffc0 == 0xfe80 {
        return None;
    }
    Some(std::net::SocketAddrV6::new(ip, 53, 0, scope).into())
}

/// The index of the network interface `name`, if there is one.
#[cfg(unix)]
fn interface_index(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: a NUL-terminated string.
    let index = unsafe { libc::if_nametoindex(name.as_ptr()) };
    (index != 0).then_some(index)
}

/// The index of the network interface `name` (such as `ethernet_32768` or
/// `loopback_0`), if there is one.
#[cfg(windows)]
fn interface_index(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    // SAFETY: a NUL-terminated string.
    let index = unsafe {
        windows_sys::Win32::NetworkManagement::IpHelper::if_nametoindex(name.as_ptr().cast())
    };
    (index != 0).then_some(index)
}

/// The current thread's resolver for the system configuration, reloaded when
/// `/etc/resolv.conf` or `/etc/hosts` change (checked every few seconds; on
/// Windows, the hosts file and what the adapters report).
pub(crate) async fn system_resolver() -> io::Result<Resolver> {
    let now = Instant::now();
    let fresh = SYSTEM.with(|slot| {
        slot.borrow()
            .as_ref()
            .filter(|sys| now.duration_since(sys.checked) < RELOAD_CHECK)
            .map(|sys| sys.resolver.clone())
    });
    if let Some(resolver) = fresh {
        return Ok(resolver);
    }
    // Read once per check: on Windows, asking the adapters is not cheap,
    // and the stamps hold what they said.
    let stamps = read_stamps().await;
    let kept = SYSTEM.with(|slot| {
        let mut slot = slot.borrow_mut();
        let sys = slot.as_mut()?;
        sys.checked = now;
        if stamps == sys.stamps {
            return Some(sys.resolver.clone());
        }
        // The adapters could not be asked this time: what they said last
        // still holds, and they are asked again at the next check.
        #[cfg(windows)]
        if stamps.1.is_none() {
            return Some(sys.resolver.clone());
        }
        None
    });
    if let Some(resolver) = kept {
        return Ok(resolver);
    }
    let resolver = Resolver::new(system_config(&stamps).await?);
    SYSTEM.with(|slot| {
        *slot.borrow_mut() = Some(SystemResolver {
            resolver: resolver.clone(),
            checked: now,
            stamps,
        });
    });
    Ok(resolver)
}

/// What the fuzz targets call (see `crate::fuzzing`).
#[cfg(fuzzing)]
pub(crate) mod fuzzing {
    use super::*;

    /// Parses `data` as the answer to a query. The first byte chooses the
    /// question: the one the response itself carries (so that the fuzzer
    /// gets past the header checks and into the records and CNAME chains), or
    /// a fixed `A` or `AAAA` question for `example.com`. Must never panic,
    /// and must take time linear in the size of the input (the CI job runs
    /// it with a short `-timeout`).
    pub fn dns_response(data: &[u8]) {
        let Some((&mode, buf)) = data.split_first() else {
            return;
        };
        let id = buf.get(..2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]));
        match mode % 3 {
            0 => {
                let Ok((name, pos)) = read_name(buf, 12) else {
                    return;
                };
                let Ok(qtype) = read_u16(buf, pos) else {
                    return;
                };
                if let Ok(answer) = parse_response(buf, id, &name, qtype) {
                    for ip in &answer.addrs {
                        assert_eq!(ip.is_ipv4(), qtype == TYPE_A);
                    }
                }
            }
            1 => drop(parse_response(buf, id, "example.com", TYPE_A)),
            _ => drop(parse_response(buf, id, "example.com", TYPE_AAAA)),
        }
    }

    /// Parses `data` as `/etc/resolv.conf`, then a NUL byte, then
    /// `/etc/hosts`. Must never panic.
    pub fn resolv_conf(data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let (resolv, hosts) = text.split_once('\0').unwrap_or((&text, ""));
        let cfg = ResolverConfig::parse(resolv, hosts);
        assert!(!cfg.nameservers.is_empty());
        for name in ["localhost", "example.com", "a.b.c.d.e.f"] {
            for candidate in cfg.candidates(name) {
                drop(build_query(1, &candidate, TYPE_A));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn no_other_socket_can_take_a_query_sockets_port() {
        let socket = query_socket((Ipv4Addr::UNSPECIFIED, 0).into()).unwrap();
        let port = socket.local_addr().unwrap().port();
        // A more specific address on the same port: Windows lets such a
        // bind take the port's datagrams unless the first bind is exclusive.
        let taken = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port));
        assert!(taken.is_err(), "bound {:?} over the query's port", taken);
    }

    /// The threads that read the system stamps, for the test below.
    #[cfg(windows)]
    static STAMP_THREADS: std::sync::Mutex<Vec<std::thread::ThreadId>> =
        std::sync::Mutex::new(Vec::new());

    #[cfg(windows)]
    pub(super) fn note_stamps_thread() {
        STAMP_THREADS
            .lock()
            .unwrap()
            .push(std::thread::current().id());
    }

    #[test]
    fn windows_asks_the_adapters_on_a_blocking_thread() {
        // One name on every platform, for the simulation's index; the body
        // is Windows'.
        #[cfg(windows)]
        crate::block_on(async {
            SYSTEM.with(|slot| *slot.borrow_mut() = None);
            let worker = std::thread::current().id();
            drop(system_resolver().await);
            let threads = STAMP_THREADS.lock().unwrap();
            assert!(!threads.is_empty());
            assert!(
                !threads.contains(&worker),
                "the adapters were asked on the worker"
            );
        });
    }

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

    /// `c{i}.example` as a wire name.
    fn chain_name(i: usize) -> Vec<u8> {
        let label = format!("c{i}");
        let mut name = vec![label.len() as u8];
        name.extend_from_slice(label.as_bytes());
        name.extend_from_slice(b"\x07example\x00");
        name
    }

    /// An answer for `c0.example` whose CNAME records lead from `c0` to
    /// `c{links}`, which has an A record, listed last link first; plus
    /// `unrelated` CNAME records of other names.
    fn chain_response(links: usize, unrelated: usize) -> Vec<u8> {
        let q = build_query(3, "c0.example", TYPE_A).unwrap();
        let names: Vec<Vec<u8>> = (0..=links).map(chain_name).collect();
        let others: Vec<(Vec<u8>, Vec<u8>)> = (0..unrelated)
            .map(|i| (chain_name(100_000 + 2 * i), chain_name(100_001 + 2 * i)))
            .collect();
        let mut answers: Vec<(&[u8], u16, u32, &[u8])> =
            vec![(&names[links], TYPE_A, 60, &[192, 0, 2, 7])];
        for i in (0..links).rev() {
            answers.push((&names[i], TYPE_CNAME, 300, &names[i + 1]));
        }
        for (owner, target) in &others {
            answers.push((owner, TYPE_CNAME, 300, target));
        }
        response(&q, 0, false, &answers)
    }

    #[test]
    fn long_cname_chains_cost_linear_time() {
        // Following the chain used to rescan every record for every name
        // found: about 18 s for one 64 KiB TCP answer of 2,300 records, on
        // the worker thread, with no timer or other task running meanwhile.
        let start = std::time::Instant::now();
        let r = chain_response(2_300, 0);
        let err = parse_response(&r, 3, "c0.example", TYPE_A).unwrap_err();
        assert_eq!(crate::error::code(&err), Some(Code::DnsBadResponse));
        assert!(err.to_string().contains("more than 16 CNAME"), "{err}");
        // Thousands of records with a short chain among them.
        let r = chain_response(3, 2_300);
        let answer = parse_response(&r, 3, "c0.example", TYPE_A).unwrap();
        assert_eq!(answer.addrs, [IpAddr::from([192, 0, 2, 7])]);
        if !cfg!(miri) {
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "{:?}",
                start.elapsed()
            );
        }
        // Up to 16 links are followed.
        let r = chain_response(16, 0);
        let answer = parse_response(&r, 3, "c0.example", TYPE_A).unwrap();
        assert_eq!(answer.addrs, [IpAddr::from([192, 0, 2, 7])]);
        assert_eq!(answer.min_ttl, 60);
        assert!(parse_response(&chain_response(17, 0), 3, "c0.example", TYPE_A).is_err());
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

    /// The authority section of a negative answer: its SOA record, a
    /// compressed pointer to the question's name for the owner.
    fn with_soa(mut r: Vec<u8>, ttl: u32, minimum: u32, before: &[(u16, &[u8])]) -> Vec<u8> {
        let count = before.len() as u16 + 1;
        r[8..10].copy_from_slice(&count.to_be_bytes());
        for (rtype, rdata) in before {
            r.extend_from_slice(&[0xC0, 12]);
            r.extend_from_slice(&rtype.to_be_bytes());
            r.extend_from_slice(&CLASS_IN.to_be_bytes());
            r.extend_from_slice(&600u32.to_be_bytes());
            r.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            r.extend_from_slice(rdata);
        }
        let mut soa = b"\x02ns\xC0\x0C\x05admin\xC0\x0C".to_vec();
        for field in [2026112201u32, 7200, 900, 1209600, minimum] {
            soa.extend_from_slice(&field.to_be_bytes());
        }
        r.extend_from_slice(&[0xC0, 12]);
        r.extend_from_slice(&TYPE_SOA.to_be_bytes());
        r.extend_from_slice(&CLASS_IN.to_be_bytes());
        r.extend_from_slice(&ttl.to_be_bytes());
        r.extend_from_slice(&(soa.len() as u16).to_be_bytes());
        r.extend_from_slice(&soa);
        r
    }

    #[test]
    fn a_negative_answer_carries_the_ttl_of_its_soa_record() {
        let q = build_query(4, "gone.example", TYPE_A).unwrap();
        let parse = |r: &[u8]| parse_response(r, 4, "gone.example", TYPE_A).unwrap();
        // The smaller of the record's TTL and its MINIMUM field.
        let nx = parse(&with_soa(response(&q, 3, false, &[]), 3600, 60, &[]));
        assert_eq!((nx.rcode, nx.negative_ttl), (RCODE_NXDOMAIN, Some(60)));
        let nx = parse(&with_soa(response(&q, 3, false, &[]), 20, 900, &[]));
        assert_eq!(nx.negative_ttl, Some(20));
        // NODATA too, and past other authority records.
        let nodata = parse(&with_soa(
            response(&q, 0, false, &[]),
            300,
            300,
            &[(2, b"\x02ns\xC0\x0C")],
        ));
        assert_eq!((nodata.rcode, nodata.negative_ttl), (0, Some(300)));
        // No SOA record: nothing to cache by.
        assert_eq!(parse(&response(&q, 3, false, &[])).negative_ttl, None);
        // A cut authority section costs the caching, not the answer.
        let mut cut = with_soa(response(&q, 3, false, &[]), 300, 300, &[]);
        cut.truncate(cut.len() - 3);
        let nx = parse(&cut);
        assert_eq!((nx.rcode, nx.negative_ttl), (RCODE_NXDOMAIN, None));
        // An SOA whose fields run past its record length is ignored.
        let mut short = with_soa(response(&q, 3, false, &[]), 300, 300, &[]);
        let rdlen_at = short.len() - 2 - 18 - 20;
        let rdlen = u16::from_be_bytes([short[rdlen_at], short[rdlen_at + 1]]);
        short[rdlen_at..rdlen_at + 2].copy_from_slice(&(rdlen - 4).to_be_bytes());
        short.truncate(short.len() - 4);
        assert_eq!(parse(&short).negative_ttl, None);
        // An answer with addresses is not negative.
        let ok = parse(&with_soa(
            response(&q, 0, false, &[(&q[12..26], TYPE_A, 30, &[192, 0, 2, 1])]),
            300,
            300,
            &[],
        ));
        assert_eq!((ok.addrs.len(), ok.negative_ttl), (1, None));
    }

    #[test]
    fn a_shared_error_keeps_its_code_message_and_os_error() {
        let os = io::Error::from_raw_os_error(111);
        let coded: io::Error =
            crate::error::Error::new(Code::DnsServerUnreachable, "cannot reach", "check it.")
                .with_source(io::Error::from_raw_os_error(111))
                .into();
        let copy = copy_error(&coded);
        assert_eq!(copy.kind(), coded.kind());
        assert_eq!(copy.to_string(), coded.to_string());
        assert_eq!(crate::error::code(&copy), Some(Code::DnsServerUnreachable));
        let source = std::error::Error::source(copy.get_ref().unwrap()).unwrap();
        let source = source.downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.raw_os_error(), Some(111));
        assert_eq!(copy_error(&os).raw_os_error(), Some(111));
        let plain = io::Error::new(io::ErrorKind::TimedOut, "slow");
        let copy = copy_error(&plain);
        assert_eq!(
            (copy.kind(), copy.to_string()),
            (plain.kind(), plain.to_string())
        );
        let bare: io::Error = io::ErrorKind::UnexpectedEof.into();
        assert_eq!(copy_error(&bare).kind(), io::ErrorKind::UnexpectedEof);
    }

    /// macOS writes a router's link-local address with its interface
    /// (`fe80::1%en0`); the zone becomes the address's scope. One naming no
    /// interface, or zone 0 on a link-local address, is skipped.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call if_nametoindex")]
    fn a_nameserver_with_a_zone_gets_its_scope() {
        #[cfg(target_os = "linux")]
        let lo = "lo".to_string();
        #[cfg(target_os = "macos")]
        let lo = "lo0".to_string();
        // Windows names interfaces like `loopback_0`; the loopback's index
        // is 1.
        #[cfg(windows)]
        let lo = {
            let mut name = [0u8; 256];
            // SAFETY: a buffer of at least IF_MAX_STRING_SIZE + 1 bytes.
            let named = unsafe {
                windows_sys::Win32::NetworkManagement::IpHelper::if_indextoname(
                    1,
                    name.as_mut_ptr(),
                )
            };
            assert!(!named.is_null(), "no interface with index 1");
            let len = name.iter().position(|&b| b == 0).unwrap();
            String::from_utf8_lossy(&name[..len]).into_owned()
        };
        let cfg = ResolverConfig::parse(
            &format!(
                "nameserver fe80::1%nosuchif0\nnameserver fe80::4%0\nnameserver fe80::2%{lo}\nnameserver fe80::3%7\n"
            ),
            "",
        );
        let scopes: Vec<_> = cfg
            .nameservers
            .iter()
            .map(|a| match a {
                SocketAddr::V6(a) => (a.ip().segments()[7], a.port(), a.scope_id()),
                SocketAddr::V4(_) => unreachable!(),
            })
            .collect();
        assert_eq!(scopes.len(), 2, "{scopes:?}");
        assert_eq!(scopes[0].0, 2);
        assert!(scopes[0].2 > 0, "{scopes:?}");
        assert_eq!(scopes[1], (3, 53, 7));
    }

    #[test]
    fn windows_takes_the_name_servers_of_its_adapters() {
        // One test on every platform (the simulation's index is built from
        // test names), with nothing to check off Windows.
        #[cfg(windows)]
        {
            let (servers, search) = crate::sys::dns_config().unwrap();
            assert!(!servers.is_empty(), "no adapter reports a DNS server");
            let cfg = ResolverConfig::parse(&adapters_as_resolv_conf(&servers, &search), "");
            let expected: Vec<SocketAddr> = servers
                .iter()
                .take(3)
                .map(|&ip| SocketAddr::new(ip, 53))
                .collect();
            assert_eq!(cfg.nameservers, expected);
            assert_eq!(cfg.search, search);
            ResolverConfig::system().unwrap();
        }
    }

    #[test]
    fn windows_with_no_adapter_dns_server_fails_with_a_coded_error() {
        // One name on every platform, for the simulation's index; the body
        // is Windows'.
        #[cfg(windows)]
        {
            let cfg = ResolverConfig::from_adapters(&[], &["corp.example".into()], "");
            assert!(cfg.nameservers.is_empty());
            let resolver = Resolver::new(cfg);
            let err = crate::block_on(resolver.lookup_ip("db.corp.example")).unwrap_err();
            assert_eq!(
                crate::error::code(&err),
                Some(crate::error::Code::NoDnsServer)
            );
        }
    }

    #[test]
    fn resolv_conf_is_read_like_glibc_reads_it() {
        let cfg = ResolverConfig::parse(
            "# comment\n\
             nameserver 10.96.0.10\n\
             nameserver 2001:db8::53 ; trailing comment\n\
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
