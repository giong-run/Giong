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
    ServFail,
    /// Over UDP: an empty answer with the truncation bit. Over TCP: these.
    Truncated(Vec<(String, u16, u32, Vec<u8>)>),
    /// No answer at all.
    Silent,
}

/// Answers from a table keyed by (name, type); unknown names get NXDOMAIN.
/// Counts the queries it receives per name.
struct FakeDns {
    addr: SocketAddr,
    queries: Arc<Mutex<Vec<(String, u16)>>>,
}

impl FakeDns {
    fn start(table: Vec<((&str, u16), Reply)>) -> Self {
        let table: HashMap<(String, u16), Reply> = table
            .into_iter()
            .map(|((name, t), reply)| ((name.to_string(), t), reply))
            .collect();
        let table = Arc::new(table);
        // A UDP port whose TCP twin is free too (another test may hold it).
        let (udp, tcp, addr) = (0..100)
            .find_map(|_| {
                let udp = StdUdpSocket::bind("127.0.0.1:0").unwrap();
                let addr = udp.local_addr().unwrap();
                let tcp = StdTcpListener::bind(addr).ok()?;
                Some((udp, tcp, addr))
            })
            .expect("no free UDP and TCP port pair");
        let queries = Arc::new(Mutex::new(Vec::new()));

        let (t, q) = (table.clone(), queries.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 512];
            while let Ok((n, from)) = udp.recv_from(&mut buf) {
                let Some(reply) = answer(&buf[..n], &t, &q, false) else {
                    continue;
                };
                let _ = udp.send_to(&reply, from);
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
                if let Some(reply) = answer(&query, &t, &q, true) {
                    let mut framed = (reply.len() as u16).to_be_bytes().to_vec();
                    framed.extend_from_slice(&reply);
                    let _ = stream.write_all(&framed);
                }
            }
        });
        Self { addr, queries }
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
) -> Option<Vec<u8>> {
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
    let (rcode, tc, records) = match reply {
        Reply::Records(r) => (0u16, false, r),
        Reply::NxDomain => (3, false, Vec::new()),
        Reply::ServFail => (2, false, Vec::new()),
        Reply::Truncated(r) if over_tcp => (0, false, r),
        Reply::Truncated(_) => (0, true, Vec::new()),
        Reply::Silent => return None,
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
    Some(out)
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
