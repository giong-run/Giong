//! RFC 9218 priorities over HTTP/3: `Server::serve_h3` schedules responses
//! by the urgency of their requests. Through a relay that holds the path
//! to a set rate, as a real link does, an urgent response asked for while
//! large ones are on their way comes first.
#![cfg(all(feature = "client", feature = "unstable-http3"))]

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giong_http::client::Client;
use giong_http::{Handler, Request, Response, Server};
use giong_quic::{ClientConfig, Endpoint, ServerConfig};

fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key.into())
        .unwrap();
    server.alpn_protocols = vec![b"h3".to_vec()];
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"h3".to_vec()];
    (server, client)
}

struct App;

impl Handler for App {
    async fn call(&self, req: Request) -> Response {
        match req
            .path()
            .strip_prefix("/bytes/")
            .and_then(|n| n.parse().ok())
        {
            Some(n) => {
                let resp = Response::new(200).body(vec![b'x'; n]);
                // The handler's own say (RFC 9218, section 8).
                match req.query() {
                    Some("urgent") => resp.header("priority", "u=0"),
                    _ => resp,
                }
            }
            None => Response::new(404),
        }
    }
}

/// The path: its rate one way (server to client), its delay each way, and
/// the most a datagram may queue before it is dropped, as at a router.
#[derive(Clone, Copy)]
struct Link {
    bytes_per_sec: u64,
    delay: Duration,
    max_queue: Duration,
}

/// Forwards datagrams from `from` to wherever `to` gives, after `delay`;
/// paced to `rate` bytes a second when there is one.
fn forward(
    from: Arc<UdpSocket>,
    out: Arc<UdpSocket>,
    to: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + 'static,
    rate: Option<u64>,
    link: Link,
) {
    let (tx, rx) = mpsc::channel::<(Instant, Vec<u8>, SocketAddr)>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        // When the link is free again.
        let mut free = Instant::now();
        while let Ok((n, peer)) = from.recv_from(&mut buf) {
            let Some(dest) = to(peer) else { continue };
            let now = Instant::now();
            let leaves = match rate {
                Some(rate) => {
                    let start = free.max(now);
                    if start - now > link.max_queue {
                        continue;
                    }
                    free = start + Duration::from_nanos(n as u64 * 1_000_000_000 / rate);
                    free
                }
                None => now,
            };
            if tx
                .send((leaves + link.delay, buf[..n].to_vec(), dest))
                .is_err()
            {
                return;
            }
        }
    });
    std::thread::spawn(move || {
        let mut queue = VecDeque::new();
        while let Ok(item) = rx.recv() {
            queue.push_back(item);
            while let Ok(item) = rx.try_recv() {
                queue.push_back(item);
            }
            while let Some((at, datagram, dest)) = queue.pop_front() {
                let now = Instant::now();
                if at > now {
                    std::thread::sleep(at - now);
                }
                let _ = out.send_to(&datagram, dest);
                while let Ok(item) = rx.try_recv() {
                    queue.push_back(item);
                }
            }
        }
    });
}

/// A relay to `server` over `link`; the address to connect to.
fn relay(server: SocketAddr, link: Link) -> SocketAddr {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    let addr = front.local_addr().unwrap();
    let client = Arc::new(Mutex::new(None));
    let seen = client.clone();
    let to_server = move |peer| {
        *seen.lock().unwrap() = Some(peer);
        Some(server)
    };
    forward(front.clone(), back.clone(), to_server, None, link);
    let to_client = move |_| *client.lock().unwrap();
    forward(back, front, to_client, Some(link.bytes_per_sec), link);
    addr
}

/// How long the late response took, from its request, and how long each
/// of the `bulk` responses took, from theirs.
struct Timings {
    late: Duration,
    bulk: Vec<Duration>,
}

/// `bulk` responses of `size` bytes, then, `after` later, one more of
/// `late_size` asked for with `late_query` and the request's `priority`
/// field if any.
fn race(
    link: Link,
    bulk: usize,
    size: usize,
    late_size: usize,
    after: Duration,
    priority: Option<&'static str>,
    late_query: &'static str,
) -> Timings {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async move {
            let (server_tls, client_tls) = tls();
            let endpoint = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(server_tls).unwrap(),
            )
            .unwrap();
            let addr = relay(endpoint.local_addr().unwrap(), link);
            drop(giong::spawn(async move {
                Server::new().serve_h3(endpoint, App).await
            }));
            let client = Client::builder()
                .http3_prior_knowledge(ClientConfig::new(client_tls).unwrap())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .build();
            let url = format!("https://{addr}");
            // The connection first, so that every request shares it.
            client.get(&format!("{url}/bytes/1")).send().await.unwrap();
            let get = |path: String, priority: Option<&'static str>| {
                let client = client.clone();
                giong::spawn(async move {
                    let start = Instant::now();
                    let mut req = client.get(&path);
                    if let Some(p) = priority {
                        req = req.header("priority", p);
                    }
                    let res = req.send().await.unwrap();
                    assert_eq!(res.status(), 200);
                    start.elapsed()
                })
            };
            let tasks: Vec<_> = (0..bulk)
                .map(|_| get(format!("{url}/bytes/{size}"), None))
                .collect();
            giong::time::sleep(after).await;
            let late = get(format!("{url}/bytes/{late_size}?{late_query}"), priority);
            let late = late.await.unwrap();
            let mut times = Vec::new();
            for t in tasks {
                times.push(t.await.unwrap());
            }
            Timings { late, bulk: times }
        })
        .unwrap()
}

const LINK: Link = Link {
    bytes_per_sec: 8_000_000,
    delay: Duration::from_millis(10),
    max_queue: Duration::from_millis(200),
};

#[test]
fn an_urgent_response_overtakes_those_on_their_way() {
    // Four responses of 1 MB, then one more of 1 MB asked for as urgent:
    // it comes first, though asked for last.
    let t = race(
        LINK,
        4,
        1 << 20,
        1 << 20,
        Duration::from_millis(100),
        Some("u=0"),
        "",
    );
    let first_bulk = t.bulk.iter().min().unwrap();
    assert!(
        t.late + Duration::from_millis(100) < *first_bulk,
        "urgent {:?}, the others {:?}",
        t.late,
        t.bulk
    );
}

#[test]
fn a_handler_can_make_its_response_urgent() {
    let t = race(
        LINK,
        4,
        1 << 20,
        1 << 20,
        Duration::from_millis(100),
        None,
        "urgent",
    );
    let first_bulk = t.bulk.iter().min().unwrap();
    assert!(
        t.late + Duration::from_millis(100) < *first_bulk,
        "urgent {:?}, the others {:?}",
        t.late,
        t.bulk
    );
}

/// The measurement in `bench/results/h3-priorities`: run with
/// `--ignored --nocapture`.
#[test]
#[ignore = "a measurement: run it with --ignored --nocapture"]
fn measure_urgent_response_times() {
    for round in 1..=3 {
        for (name, priority) in [("no priority field", None), ("priority: u=0", Some("u=0"))] {
            for late_size in [16 << 10, 256 << 10, 1 << 20] {
                let t = race(
                    LINK,
                    4,
                    1 << 20,
                    late_size,
                    Duration::from_millis(100),
                    priority,
                    "",
                );
                let mean = t.bulk.iter().sum::<Duration>() / t.bulk.len() as u32;
                println!(
                    "round {round} | {name} | late {late_size} B | late {:.1} ms | bulk mean {:.1} ms",
                    t.late.as_secs_f64() * 1e3,
                    mean.as_secs_f64() * 1e3
                );
            }
        }
    }
}
