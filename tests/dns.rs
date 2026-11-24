//! Name resolution against a small fake DNS server (UDP and TCP on one port),
//! and connecting by name.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener as StdTcpListener, UdpSocket as StdUdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giong::error::{code, Code};
use giong::net::{lookup_host, Resolver, ResolverConfig, TcpListener, TcpStream};

mod common;

const A: u16 = 1;
const CNAME: u16 = 5;
const AAAA: u16 = 28;

#[derive(Clone)]
enum Reply {
    /// Answer records: (owner name, type, ttl, data).
    Records(Vec<(String, u16, u32, Vec<u8>)>),
    NxDomain,
    /// NXDOMAIN with the zone's SOA record (its TTL and MINIMUM fields) in
    /// the authority section, as recursive servers send it.
    NxDomainSoa(u32, u32),
    ServFail,
    /// Over UDP: an empty answer with the truncation bit. Over TCP: these.
    Truncated(Vec<(String, u16, u32, Vec<u8>)>),
    /// No answer at all.
    Silent,
    /// These records, sent this long after the query.
    Late(Duration, Vec<(String, u16, u32, Vec<u8>)>),
}

/// Answers from a table keyed by (name, type); unknown names get NXDOMAIN.
/// Counts the queries it receives per name.
struct FakeDns {
    addr: SocketAddr,
    queries: Arc<Mutex<Vec<(String, u16)>>>,
    /// What the UDP side did and when, for a test to print when its answer
    /// is not what the timing should give.
    events: Arc<Mutex<Vec<(Instant, String)>>>,
}

impl FakeDns {
    fn start(table: Vec<((&str, u16), Reply)>) -> Self {
        let table: HashMap<(String, u16), Reply> = table
            .into_iter()
            .map(|((name, t), reply)| ((name.to_string(), t), reply))
            .collect();
        let table = Arc::new(table);
        // A port free for both TCP and UDP (another test may hold either).
        // Tries alternate between a fresh TCP port with its UDP twin and a
        // fresh UDP port with its TCP twin, as each way fails in runs on
        // Windows, which hands out ports in sequence: the TCP twin of a
        // fresh UDP port can be one this suite's connections left in
        // TIME_WAIT, and on a runner 100 fresh TCP ports in a row had their
        // UDP twins refused. Sockets whose twin is busy stay open until a
        // pair is found: closed at once, the next bind may be given the
        // same port again.
        let (mut busy_tcp, mut busy_udp, mut refusal) = (Vec::new(), Vec::new(), None);
        let (udp, tcp, addr) = (0..200)
            .find_map(|i| {
                if i % 2 == 0 {
                    let tcp = StdTcpListener::bind("127.0.0.1:0").unwrap();
                    let addr = tcp.local_addr().unwrap();
                    match StdUdpSocket::bind(addr) {
                        Ok(udp) => Some((udp, tcp, addr)),
                        Err(err) => {
                            refusal = Some(err);
                            busy_tcp.push(tcp);
                            None
                        }
                    }
                } else {
                    let udp = StdUdpSocket::bind("127.0.0.1:0").unwrap();
                    let addr = udp.local_addr().unwrap();
                    match StdTcpListener::bind(addr) {
                        Ok(tcp) => Some((udp, tcp, addr)),
                        Err(err) => {
                            refusal = Some(err);
                            busy_udp.push(udp);
                            None
                        }
                    }
                }
            })
            .unwrap_or_else(|| panic!("no free UDP and TCP port pair; last refusal: {refusal:?}"));
        drop((busy_tcp, busy_udp));
        let queries = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));

        let (t, q, ev) = (table.clone(), queries.clone(), events.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            loop {
                let (n, from) = match udp.recv_from(&mut buf) {
                    Ok(got) => got,
                    // Windows reports a "port unreachable" answer to an
                    // earlier reply (its asker had gone) on the next
                    // receive; the server carries on.
                    Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => {
                        record(&ev, format!("receive: {err}"));
                        continue;
                    }
                    Err(_) => return,
                };
                // A response (the QR bit set) is not a query. Windows hands
                // out ports in sequence, so this server's port can be one
                // that a query socket of another test process held, and a
                // late answer to that query lands here.
                if buf[..n].get(2).is_some_and(|flags| flags & 0x80 != 0) {
                    record(&ev, format!("a response from {from}, ignored"));
                    continue;
                }
                let Some((reply, delay)) = answer(&buf[..n], &t, &q, false) else {
                    continue;
                };
                let what = format!("type {} for {from}", qtype_of(&reply));
                record(&ev, format!("query {what}"));
                if delay.is_zero() {
                    let sent = udp.send_to(&reply, from);
                    record(&ev, format!("answer {what} sent: {sent:?}"));
                } else {
                    let (udp, ev) = (udp.try_clone().unwrap(), ev.clone());
                    std::thread::spawn(move || {
                        std::thread::sleep(delay);
                        let sent = udp.send_to(&reply, from);
                        record(&ev, format!("answer {what} sent: {sent:?}"));
                    });
                }
            }
        });
        let (t, q) = (table, queries.clone());
        std::thread::spawn(move || {
            for stream in tcp.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut len = [0u8; 2];
                if stream.read_exact(&mut len).is_err() {
                    continue;
                }
                let mut query = vec![0u8; u16::from_be_bytes(len) as usize];
                if stream.read_exact(&mut query).is_err() {
                    continue;
                }
                if let Some((reply, _)) = answer(&query, &t, &q, true) {
                    let mut framed = (reply.len() as u16).to_be_bytes().to_vec();
                    framed.extend_from_slice(&reply);
                    let _ = stream.write_all(&framed);
                }
            }
        });
        Self {
            addr,
            queries,
            events,
        }
    }

    /// The UDP side's events, each with its time since `since`.
    fn timeline(&self, since: Instant) -> String {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|(at, what)| {
                format!(
                    "\n  {:>4} ms  {what}",
                    at.saturating_duration_since(since).as_millis()
                )
            })
            .collect()
    }

    fn count(&self, name: &str) -> usize {
        self.queries
            .lock()
            .unwrap()
            .iter()
            .filter(|(n, _)| n == name)
            .count()
    }

    fn names(&self) -> Vec<String> {
        self.queries
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn resolver(&self, resolv_conf: &str) -> Resolver {
        let cfg = ResolverConfig::parse(resolv_conf, "")
            .nameservers([self.addr])
            .timeout(Duration::from_secs(5));
        Resolver::new(cfg)
    }
}

fn record(events: &Mutex<Vec<(Instant, String)>>, what: String) {
    events.lock().unwrap().push((Instant::now(), what));
}

/// The type in the question of a query or an answer to it.
fn qtype_of(message: &[u8]) -> u16 {
    let mut pos = 12;
    while message[pos] != 0 {
        pos += 1 + message[pos] as usize;
    }
    u16::from_be_bytes([message[pos + 1], message[pos + 2]])
}

fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in name.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

fn answer(
    query: &[u8],
    table: &HashMap<(String, u16), Reply>,
    log: &Mutex<Vec<(String, u16)>>,
    over_tcp: bool,
) -> Option<(Vec<u8>, Duration)> {
    // The question: labels from offset 12, then type and class.
    let mut pos = 12;
    let mut labels = Vec::new();
    while *query.get(pos)? != 0 {
        let len = query[pos] as usize;
        labels.push(
            std::str::from_utf8(query.get(pos + 1..pos + 1 + len)?)
                .ok()?
                .to_string(),
        );
        pos += 1 + len;
    }
    let qtype = u16::from_be_bytes([query[pos + 1], query[pos + 2]]);
    let name = labels.join(".").to_ascii_lowercase();
    log.lock().unwrap().push((name.clone(), qtype));
    let end = pos + 5;

    let reply = table
        .get(&(name.clone(), qtype))
        .cloned()
        .unwrap_or_else(|| {
            // A name with records of the other type exists: NODATA, not NXDOMAIN.
            let other = if qtype == A { AAAA } else { A };
            if table.contains_key(&(name.clone(), other)) {
                Reply::Records(Vec::new())
            } else {
                Reply::NxDomain
            }
        });
    let (mut delay, mut soa) = (Duration::ZERO, None);
    let (rcode, tc, records) = match reply {
        Reply::Records(r) => (0u16, false, r),
        Reply::NxDomain => (3, false, Vec::new()),
        Reply::NxDomainSoa(ttl, minimum) => {
            soa = Some((ttl, minimum));
            (3, false, Vec::new())
        }
        Reply::ServFail => (2, false, Vec::new()),
        Reply::Truncated(r) if over_tcp => (0, false, r),
        Reply::Truncated(_) => (0, true, Vec::new()),
        Reply::Silent => return None,
        Reply::Late(after, r) => {
            delay = after;
            (0, false, r)
        }
    };
    let mut out = query[..end].to_vec();
    let flags = 0x8180u16 | rcode | if tc { 0x0200 } else { 0 };
    out[2..4].copy_from_slice(&flags.to_be_bytes());
    out[6..8].copy_from_slice(&(records.len() as u16).to_be_bytes());
    for (owner, rtype, ttl, data) in records {
        out.extend_from_slice(&encode_name(&owner));
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&ttl.to_be_bytes());
        let data = if rtype == CNAME {
            encode_name(std::str::from_utf8(&data).unwrap())
        } else {
            data
        };
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(&data);
    }
    if let Some((ttl, minimum)) = soa {
        out[8..10].copy_from_slice(&1u16.to_be_bytes());
        // The zone is the query name's parent (or the root).
        let zone = name.split_once('.').map_or("", |(_, zone)| zone);
        out.extend_from_slice(&encode_name(zone));
        out.extend_from_slice(&6u16.to_be_bytes());
        out.extend_from_slice(&1u16.to_be_bytes());
        out.extend_from_slice(&ttl.to_be_bytes());
        let mut data = encode_name(&format!("ns.{zone}"));
        data.extend_from_slice(&encode_name(&format!("admin.{zone}")));
        for field in [1u32, 7200, 900, 86400, minimum] {
            data.extend_from_slice(&field.to_be_bytes());
        }
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(&data);
    }
    Some((out, delay))
}

fn a(name: &str, ip: [u8; 4], ttl: u32) -> (String, u16, u32, Vec<u8>) {
    (name.to_string(), A, ttl, ip.to_vec())
}

fn aaaa(name: &str, ip: &str, ttl: u32) -> (String, u16, u32, Vec<u8>) {
    let ip: std::net::Ipv6Addr = ip.parse().unwrap();
    (name.to_string(), AAAA, ttl, ip.octets().to_vec())
}

fn ips(list: &[&str]) -> Vec<IpAddr> {
    list.iter().map(|s| s.parse().unwrap()).collect()
}

// -------------------------------------------------------------------- tests

#[test]
fn resolves_ipv6_and_ipv4_addresses() {
    let dns = FakeDns::start(vec![
        (
            ("api.test", A),
            Reply::Records(vec![a("api.test", [192, 0, 2, 1], 60)]),
        ),
        (
            ("api.test", AAAA),
            Reply::Records(vec![aaaa("api.test", "2001:db8::1", 60)]),
        ),
    ]);
    giong::block_on(async {
        let r = dns.resolver("");
        assert_eq!(
            r.lookup_ip("api.test").await.unwrap(),
            ips(&["2001:db8::1", "192.0.2.1"])
        );
        // Upper case and a trailing dot are the same name.
        assert_eq!(r.lookup_ip("API.test.").await.unwrap().len(), 2);
        // IP literals need no server.
        assert_eq!(r.lookup_ip("10.1.2.3").await.unwrap(), ips(&["10.1.2.3"]));
        assert_eq!(r.lookup_ip("[::1]").await.unwrap(), ips(&["::1"]));
    });
}

#[test]
fn follows_a_cname_to_its_addresses() {
    let dns = FakeDns::start(vec![(
        ("alias.test", A),
        Reply::Records(vec![
            ("alias.test".into(), CNAME, 300, b"real.test".to_vec()),
            a("real.test", [198, 51, 100, 7], 300),
        ]),
    )]);
    giong::block_on(async {
        let r = dns.resolver("");
        assert_eq!(
            r.lookup_ip("alias.test").await.unwrap(),
            ips(&["198.51.100.7"])
        );
    });
}

#[test]
fn the_search_list_finds_kubernetes_service_names() {
    let dns = FakeDns::start(vec![(
        ("postgres.default.svc.cluster.local", A),
        Reply::Records(vec![a(
            "postgres.default.svc.cluster.local",
            [10, 96, 4, 2],
            30,
        )]),
    )]);
    giong::block_on(async {
        let r =
            dns.resolver("search default.svc.cluster.local svc.cluster.local\noptions ndots:5\n");
        assert_eq!(r.lookup_ip("postgres").await.unwrap(), ips(&["10.96.4.2"]));
        // The first search domain was asked first, and nothing after it.
        assert_eq!(dns.names(), ["postgres.default.svc.cluster.local"; 2]);
    });
}

#[test]
fn a_missing_name_is_not_found() {
    let dns = FakeDns::start(vec![]);
    giong::block_on(async {
        let r = dns.resolver("search a.test\n");
        let err = r.lookup_ip("nothing.here").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound, "{err}");
        assert_eq!(code(&err), Some(Code::HostNotFound), "{err}");
        // Both the name and the name in the search domain were tried.
        assert!(dns.count("nothing.here") == 2 && dns.count("nothing.here.a.test") == 2);
    });
}

#[test]
fn a_failing_server_is_an_error_not_a_missing_name() {
    let dns = FakeDns::start(vec![
        (("broken.test", A), Reply::ServFail),
        (("broken.test", AAAA), Reply::ServFail),
    ]);
    giong::block_on(async {
        let r = Resolver::new(
            ResolverConfig::parse("", "")
                .nameservers([dns.addr])
                .attempts(1),
        );
        let err = r.lookup_ip("broken.test").await.unwrap_err();
        assert_ne!(err.kind(), ErrorKind::NotFound);
        assert!(err.to_string().contains("SERVFAIL"), "{err}");
        assert_eq!(code(&err), Some(Code::DnsServerFailure), "{err}");
    });
}

#[test]
fn a_server_nobody_listens_on_is_a_coded_error() {
    // A DNS server address where nothing listens: the query's connected UDP
    // socket gets "connection refused". That used to come back bare, so a
    // failed connect("db.internal:5432") read as if the database refused.
    let (_refusing, server) = common::refusing_udp_addr();
    giong::block_on(async {
        let r = Resolver::new(
            ResolverConfig::parse("", "")
                .nameservers([server])
                .attempts(1),
        );
        let err = r.lookup_ip("db.internal").await.unwrap_err();
        assert_eq!(code(&err), Some(Code::DnsServerUnreachable), "{err}");
        let message = err.to_string();
        assert!(message.contains(&server.to_string()), "{message}");
        assert!(message.contains("db.internal"), "{message}");
        assert_ne!(err.kind(), ErrorKind::ConnectionRefused);
        // The kernel's error is kept as the source.
        let source = std::error::Error::source(&err).expect("a source");
        let os = source
            .downcast_ref::<std::io::Error>()
            .expect("an io::Error");
        // Windows reports a refused datagram as a reset.
        let refused = if cfg!(windows) {
            ErrorKind::ConnectionReset
        } else {
            ErrorKind::ConnectionRefused
        };
        assert_eq!(os.kind(), refused);
    });
}

#[test]
fn a_silent_server_is_skipped_for_the_next_one() {
    let dns = FakeDns::start(vec![(
        ("up.test", A),
        Reply::Records(vec![a("up.test", [192, 0, 2, 9], 60)]),
    )]);
    // Receives queries and never answers.
    let silent = StdUdpSocket::bind("127.0.0.1:0").unwrap();
    giong::block_on(async {
        let cfg = ResolverConfig::parse("", "")
            .nameservers([silent.local_addr().unwrap(), dns.addr])
            .timeout(Duration::from_millis(200));
        let start = Instant::now();
        let got = Resolver::new(cfg).lookup_ip("up.test").await.unwrap();
        assert_eq!(got, ips(&["192.0.2.9"]));
        let took = start.elapsed();
        assert!(
            took >= Duration::from_millis(200) && took < Duration::from_secs(5),
            "{took:?}"
        );

        // Nobody answers at all: a timeout, after attempts × servers × timeout.
        let cfg = ResolverConfig::parse("", "")
            .nameservers([silent.local_addr().unwrap()])
            .timeout(Duration::from_millis(100))
            .attempts(2);
        let err = Resolver::new(cfg).lookup_ip("up.test").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::TimedOut, "{err}");
        assert_eq!(code(&err), Some(Code::DnsTimeout), "{err}");
    });
}

#[test]
fn a_server_that_ignores_aaaa_queries_still_gives_ipv4() {
    // The fake server stays silent for this AAAA query.
    let dns = FakeDns::start(vec![
        (
            ("v4only.test", A),
            Reply::Records(vec![a("v4only.test", [192, 0, 2, 5], 60)]),
        ),
        (("v4only.test", AAAA), Reply::Silent),
    ]);
    giong::block_on(async {
        let cfg = ResolverConfig::parse("", "")
            .nameservers([dns.addr])
            .timeout(Duration::from_millis(200));
        let got = Resolver::new(cfg).lookup_ip("v4only.test").await.unwrap();
        assert_eq!(got, ips(&["192.0.2.5"]));
    });
}

#[test]
fn a_truncated_answer_is_asked_again_over_tcp() {
    let many: Vec<_> = (1..=40).map(|i| a("big.test", [10, 0, 0, i], 60)).collect();
    let dns = FakeDns::start(vec![(("big.test", A), Reply::Truncated(many))]);
    giong::block_on(async {
        let got = dns.resolver("").lookup_ip("big.test").await.unwrap();
        assert_eq!(got.len(), 40);
        assert_eq!(got[0], IpAddr::from([10, 0, 0, 1]));
    });
}

#[test]
fn answers_are_cached_for_their_ttl() {
    let dns = FakeDns::start(vec![
        (
            ("cached.test", A),
            Reply::Records(vec![a("cached.test", [192, 0, 2, 3], 300)]),
        ),
        (
            ("uncached.test", A),
            Reply::Records(vec![a("uncached.test", [192, 0, 2, 4], 0)]),
        ),
    ]);
    giong::block_on(async {
        let r = dns.resolver("");
        for _ in 0..3 {
            r.lookup_ip("cached.test").await.unwrap();
            r.lookup_ip("uncached.test").await.unwrap();
        }
        // One A and one AAAA query for the cached name, three pairs otherwise.
        assert_eq!(dns.count("cached.test"), 2);
        assert_eq!(dns.count("uncached.test"), 6);
    });
}

#[test]
fn lookups_of_one_name_at_the_same_time_share_one_query() {
    // Late, so that every lookup starts while the queries are out; both
    // 300 ms, so that the second answer has a grace as long (the first
    // answer's time) and a slow runner waking one reply thread late does
    // not drop a family. (With 50 ms, a macOS runner returned only AAAA.)
    let dns = FakeDns::start(vec![
        (
            ("shared.test", A),
            Reply::Late(
                Duration::from_millis(300),
                vec![a("shared.test", [192, 0, 2, 7], 300)],
            ),
        ),
        (
            ("shared.test", AAAA),
            Reply::Late(
                Duration::from_millis(300),
                vec![aaaa("shared.test", "2001:db8::7", 300)],
            ),
        ),
        (("gone.test", A), Reply::ServFail),
        (("gone.test", AAAA), Reply::ServFail),
    ]);
    giong::block_on(async {
        let started = Instant::now();
        let r = Resolver::new(
            ResolverConfig::parse("", "")
                .nameservers([dns.addr])
                .attempts(1),
        );
        let lookups: Vec<_> = (0..50)
            .map(|_| {
                let r = r.clone();
                giong::spawn(async move { r.lookup_ip("shared.test").await })
            })
            .collect();
        for lookup in lookups {
            let got = lookup.await.unwrap().unwrap();
            assert_eq!(got, ips(&["2001:db8::7", "192.0.2.7"]));
        }
        assert_eq!(
            dns.count("shared.test"),
            2,
            "the server:{}",
            dns.timeline(started)
        );

        // A failure reaches every lookup that waited for it, code and all.
        let lookups: Vec<_> = (0..10)
            .map(|_| {
                let r = r.clone();
                giong::spawn(async move { r.lookup_ip("gone.test").await })
            })
            .collect();
        for lookup in lookups {
            let err = lookup.await.unwrap().unwrap_err();
            assert_eq!(code(&err), Some(Code::DnsServerFailure), "{err}");
            assert!(err.to_string().contains("SERVFAIL"), "{err}");
        }
        assert_eq!(dns.count("gone.test"), 2);
    });
}

#[test]
fn a_cancelled_lookup_hands_its_waiters_over() {
    let dns = FakeDns::start(vec![(
        ("slow.test", A),
        Reply::Late(
            Duration::from_millis(200),
            vec![a("slow.test", [192, 0, 2, 8], 300)],
        ),
    )]);
    giong::block_on(async {
        let started = Instant::now();
        let r = dns.resolver("");
        let first = {
            let r = r.clone();
            giong::spawn(async move { r.lookup_ip("slow.test").await })
        };
        // Let the first lookup send its queries, then start one that waits
        // on it, and cancel the first.
        giong::time::sleep(Duration::from_millis(20)).await;
        let second = {
            let r = r.clone();
            giong::spawn(async move { r.lookup_ip("slow.test").await })
        };
        giong::time::sleep(Duration::from_millis(20)).await;
        first.abort();
        let got = second.await.unwrap().unwrap();
        assert_eq!(got, ips(&["192.0.2.8"]));
        // The second lookup asked again.
        assert_eq!(
            dns.count("slow.test"),
            4,
            "the server:{}",
            dns.timeline(started)
        );
    });
}

#[test]
fn a_missing_name_is_remembered_for_its_negative_ttl() {
    let dns = FakeDns::start(vec![
        (("typo.test", A), Reply::NxDomainSoa(300, 1)),
        (("typo.test", AAAA), Reply::NxDomainSoa(300, 1)),
    ]);
    giong::block_on(async {
        let r = dns.resolver("");
        for _ in 0..3 {
            let err = r.lookup_ip("typo.test").await.unwrap_err();
            assert_eq!(code(&err), Some(Code::HostNotFound), "{err}");
        }
        // Asked once: the SOA record's MINIMUM (1 s) is the negative TTL.
        assert_eq!(dns.count("typo.test"), 2);
        giong::time::sleep(Duration::from_millis(1100)).await;
        r.lookup_ip("typo.test").await.unwrap_err();
        assert_eq!(dns.count("typo.test"), 4);

        // Without an SOA record a miss is not cached (RFC 2308, section 5).
        for _ in 0..3 {
            r.lookup_ip("bare.test").await.unwrap_err();
        }
        assert_eq!(dns.count("bare.test"), 6);
    });
}

#[test]
fn a_silent_aaaa_query_does_not_hold_the_ipv4_addresses() {
    let dns = FakeDns::start(vec![
        (
            ("v4.test", A),
            Reply::Records(vec![a("v4.test", [192, 0, 2, 5], 60)]),
        ),
        (("v4.test", AAAA), Reply::Silent),
    ]);
    giong::block_on(async {
        // The timeout is 5 s; the A records come after a short grace.
        let start = Instant::now();
        let got = dns.resolver("").lookup_ip("v4.test").await.unwrap();
        assert_eq!(got, ips(&["192.0.2.5"]));
        let took = start.elapsed();
        assert!(took < Duration::from_secs(2), "{took:?}");
    });
}

#[test]
fn a_slow_server_gets_a_grace_as_long_as_its_first_answer() {
    // Both answers are slow; the AAAA one comes 150 ms after the A one,
    // within the grace, which is as long as the A answer took.
    let dns = FakeDns::start(vec![
        (
            ("far.test", A),
            Reply::Late(
                Duration::from_millis(300),
                vec![a("far.test", [192, 0, 2, 6], 60)],
            ),
        ),
        (
            ("far.test", AAAA),
            Reply::Late(
                Duration::from_millis(450),
                vec![aaaa("far.test", "2001:db8::6", 60)],
            ),
        ),
    ]);
    giong::block_on(async {
        let started = Instant::now();
        let got = dns.resolver("").lookup_ip("far.test").await.unwrap();
        // On a miss, when each answer left the server and when the lookup
        // ended tell a lost or failed exchange (the lookup ends with the
        // other answer, ~450 ms) from one that never saw its answer (the
        // grace ends it, ~900 ms) and from a late server.
        assert_eq!(
            got,
            ips(&["2001:db8::6", "192.0.2.6"]),
            "the lookup ended after {:?}; the server:{}",
            started.elapsed(),
            dns.timeline(started)
        );
    });
}

#[test]
fn the_hosts_file_comes_first() {
    let dns = FakeDns::start(vec![]);
    giong::block_on(async {
        let cfg =
            ResolverConfig::parse("", "10.9.8.7 pinned.test pinned\n").nameservers([dns.addr]);
        let r = Resolver::new(cfg);
        assert_eq!(
            r.lookup_ip("pinned.test").await.unwrap(),
            ips(&["10.9.8.7"])
        );
        assert_eq!(r.lookup_ip("PINNED").await.unwrap(), ips(&["10.9.8.7"]));
        assert!(dns.names().is_empty());
    });
}

#[test]
fn localhost_names_never_reach_dns() {
    // A DNS server that would answer for localhost under the search domain,
    // as a wildcard record or a forged reply could: the database clients
    // treat `localhost` as this machine (no TLS, passwords sent in the clear),
    // so the answer must not be used, or even asked for.
    let evil = Reply::Records(vec![a("localhost.corp.example", [203, 0, 113, 9], 300)]);
    let dns = FakeDns::start(vec![
        (("localhost.corp.example", A), evil.clone()),
        (("localhost", A), evil.clone()),
        (("db.localhost.corp.example", A), evil),
    ]);
    giong::block_on(async {
        let r = dns.resolver("search corp.example\noptions ndots:1\n");
        let loopback = ips(&["::1", "127.0.0.1"]);
        for name in [
            "localhost",
            "LocalHost.",
            "db.localhost",
            "api.dev.localhost.",
        ] {
            assert_eq!(r.lookup_ip(name).await.unwrap(), loopback, "{name}");
        }
        assert!(dns.names().is_empty(), "asked DNS for {:?}", dns.names());

        // The hosts file still decides which loopback addresses, but cannot
        // point a localhost name anywhere else.
        let hosts =
            "127.0.0.1 localhost\n10.1.2.3 localhost app.localhost\n127.0.0.2 app.localhost\n";
        let cfg = ResolverConfig::parse("search corp.example\n", hosts).nameservers([dns.addr]);
        let r = Resolver::new(cfg);
        assert_eq!(r.lookup_ip("localhost").await.unwrap(), ips(&["127.0.0.1"]));
        assert_eq!(
            r.lookup_ip("app.localhost").await.unwrap(),
            ips(&["127.0.0.2"])
        );
        assert!(dns.names().is_empty(), "asked DNS for {:?}", dns.names());

        // A name that merely ends in "localhost" is an ordinary name.
        let err = r.lookup_ip("notlocalhost").await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound, "{err}");
    });
}

#[test]
fn addresses_without_a_port_or_brackets_say_how_to_write_them() {
    giong::block_on(async {
        for (text, hint) in [
            ("10.0.0.5", "\"10.0.0.5:8080\""),
            ("::1", "\"[::1]:8080\""),
            // Itself an IPv6 address: the port has to go after brackets.
            ("2001:db8::1:8080", "\"[2001:db8::1:8080]:8080\""),
            ("fe80::1%eth0:8080", "brackets"),
        ] {
            let Err(err) = TcpStream::connect(text).await else {
                panic!("{text} connected");
            };
            assert_eq!(code(&err), Some(Code::InvalidAddress), "{text}: {err}");
            assert!(err.to_string().contains(hint), "{text}: {err}");
        }
        // Written properly, they parse (and are refused or reached as usual).
        assert!(TcpStream::connect("[::1]:1")
            .await
            .err()
            .map_or(true, |e| code(&e).is_none()));
    });
}

#[test]
fn invalid_names_are_refused_before_any_query() {
    let dns = FakeDns::start(vec![]);
    giong::block_on(async {
        let r = dns.resolver("");
        for bad in ["", "a..b", "white space", "ví-dụ.vn"] {
            let err = r.lookup_ip(bad).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{bad:?}: {err}");
            assert_eq!(code(&err), Some(Code::InvalidHostName), "{bad:?}: {err}");
        }
        assert!(dns.names().is_empty());
    });
}

#[test]
fn connect_resolves_localhost_without_blocking() {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept = giong::spawn(async move { listener.accept().await.map(|_| ()) });
        let stream = TcpStream::connect(format!("localhost:{port}"))
            .await
            .unwrap();
        assert_eq!(stream.peer_addr().unwrap().port(), port);
        accept.await.unwrap().unwrap();

        let addrs = lookup_host(("localhost", 80)).await.unwrap();
        assert!(
            addrs.iter().any(|a| a.ip().is_loopback() && a.port() == 80),
            "{addrs:?}"
        );
        assert_eq!(
            lookup_host("192.0.2.1:7").await.unwrap(),
            ["192.0.2.1:7".parse().unwrap()]
        );
    });
}

#[test]
fn connect_does_not_wait_for_an_address_that_never_answers() {
    giong::block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let live = listener.local_addr().unwrap();
        let accept = giong::spawn(async move { listener.accept().await.map(|_| ()) });
        // A private address nobody answers on (or that is unreachable here):
        // either way the live address is tried after at most 250 ms.
        let dead: SocketAddr = "10.255.255.1:9".parse().unwrap();
        let start = Instant::now();
        let stream = TcpStream::connect([dead, live]).await.unwrap();
        assert_eq!(stream.peer_addr().unwrap(), live);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        accept.await.unwrap().unwrap();
    });
}

#[test]
fn connect_reports_the_last_error_when_every_address_fails() {
    giong::block_on(async {
        // Two ports with nothing listening.
        let refusing = [common::refusing_addr(), common::refusing_addr()];
        let closed: Vec<SocketAddr> = refusing.iter().map(|(_, addr)| *addr).collect();
        let err = TcpStream::connect(&closed[..]).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused, "{err}");
        let err = TcpStream::connect(&[][..] as &[SocketAddr])
            .await
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
        assert_eq!(code(&err), Some(Code::NoAddresses), "{err}");
    });
}

/// macOS writes the primary resolver of its network configuration to
/// /etc/resolv.conf: the servers read from the file are the ones the system
/// reports for it.
#[test]
#[cfg(target_os = "macos")]
fn the_system_configuration_is_the_primary_resolver_of_macos() {
    let out = std::process::Command::new("scutil")
        .arg("--dns")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    // The first resolver listed is the primary one.
    let primary: Vec<IpAddr> = text
        .split("resolver #")
        .nth(1)
        .unwrap_or("")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("nameserver["))
        .filter_map(|rest| rest.split(": ").nth(1)?.trim().parse().ok())
        .collect();
    if primary.is_empty() {
        // No network: nothing to compare.
        return;
    }
    let cfg = ResolverConfig::system().unwrap();
    let ours: Vec<IpAddr> = cfg.nameserver_addrs().iter().map(|a| a.ip()).collect();
    assert!(!ours.is_empty());
    for server in &ours {
        assert!(
            primary.contains(server),
            "{server} is not a server of the primary resolver {primary:?}"
        );
    }
}
