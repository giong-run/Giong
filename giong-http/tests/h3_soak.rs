//! Soak test for HTTP/3: `Server::serve_h3` against giong's own HTTP/3
//! client for a long time, then a check that nothing piled up.
//!
//! Client tasks make new connections all the time (a client is dropped
//! after a few requests), half of them resuming an earlier session with
//! 0-RTT. Requests mix small GETs, POSTs echoed back, large responses,
//! responses given up part-way, requests cancelled by a timeout, and POSTs
//! sent in early data, which the server must answer 425 or after the
//! handshake.
//!
//! At the end it checks that:
//! * no request saw an error it should not see and every body was whole;
//! * the descriptor count did not grow;
//! * resident memory levelled off after the warm-up.
//!
//! It runs for `GIONG_SOAK_SECS` seconds (default 60), so it is not part of
//! the default run:
//!
//! ```text
//! GIONG_SOAK_SECS=3600 cargo test --release -p giong-http --all-features --test h3_soak -- --ignored --nocapture
//! ```
#![cfg(all(feature = "client", feature = "unstable-http3"))]

use std::cell::Cell;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use giong_http::client::Client;
use giong_http::{Handler, Request, Response, Server};
use giong_quic::{ClientConfig, Endpoint, ServerConfig};

/// Client tasks.
const CLIENTS: usize = 32;

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

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + i / 253) as u8).collect()
}

struct App;

impl Handler for App {
    async fn call(&self, req: Request) -> Response {
        match req.path() {
            "/small" => Response::text("ok"),
            "/echo" => Response::new(200).body(req.body().to_vec()),
            "/big" => Response::new(200).body(pattern(1 << 20)),
            "/slow" => {
                giong::time::sleep(Duration::from_millis(200)).await;
                Response::text("slow")
            }
            _ => Response::new(404),
        }
    }
}

#[derive(Default)]
struct Counters {
    connections: Cell<u64>,
    requests: Cell<u64>,
    early_tried: Cell<u64>,
    cancelled: Cell<u64>,
    unexpected: Cell<u64>,
}

impl Counters {
    fn bump(c: &Cell<u64>) {
        c.set(c.get() + 1);
    }

    fn unexpected(&self, what: &str, e: impl std::fmt::Display) {
        if self.unexpected.get() < 20 {
            eprintln!("unexpected: {what}: {e}");
        }
        Self::bump(&self.unexpected);
    }
}

/// Open sockets and kernel objects (io_uring, eventfd, timerfd). Files are
/// left out: the server's memory watcher keeps a few open on each worker
/// thread for as long as the thread lives.
fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .filter(|target| {
            let target = target.to_string_lossy();
            target.starts_with("socket:") || target.starts_with("anon_inode:")
        })
        .count()
}

fn rss_kib() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4
}

/// xorshift, one per client task.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

async fn client_task(
    url: Rc<String>,
    quic: ClientConfig,
    early: ClientConfig,
    until: Instant,
    seed: u64,
    n: Rc<Counters>,
) {
    let mut rng = Rng(seed | 1);
    while Instant::now() < until {
        let resumed = rng.below(2) == 0;
        let client = Client::builder()
            .http3_prior_knowledge(if resumed { early.clone() } else { quic.clone() })
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build();
        Counters::bump(&n.connections);
        for _ in 0..1 + rng.below(8) {
            Counters::bump(&n.requests);
            match rng.below(6) {
                0 | 1 => match client.get(&format!("{url}/small")).send().await {
                    Ok(res) if res.status() == 200 && res.body() == b"ok" => {}
                    Ok(res) => n.unexpected("small", res.status()),
                    Err(e) => n.unexpected("small", e),
                },
                2 => {
                    let body = pattern(rng.below(200_000) as usize);
                    if resumed {
                        Counters::bump(&n.early_tried);
                    }
                    match client
                        .post(&format!("{url}/echo"))
                        .body(body.clone())
                        .send()
                        .await
                    {
                        Ok(res) if res.status() == 200 && res.body() == &body[..] => {}
                        Ok(res) => n.unexpected("echo", res.status()),
                        Err(e) => n.unexpected("echo", e),
                    }
                }
                3 => match client.get(&format!("{url}/big")).send().await {
                    Ok(res) if res.status() == 200 && res.body() == &pattern(1 << 20)[..] => {}
                    Ok(res) => n.unexpected("big", res.status()),
                    Err(e) => n.unexpected("big", e),
                },
                4 => {
                    // A response given up after its first bytes.
                    match client.get(&format!("{url}/big")).send_streaming().await {
                        Ok(mut res) => {
                            let _ = res.chunk().await;
                            Counters::bump(&n.cancelled);
                        }
                        Err(e) => n.unexpected("streaming", e),
                    }
                }
                _ => {
                    // A request dropped by a timeout before its answer.
                    let req = client.get(&format!("{url}/slow")).send();
                    if giong::time::timeout(Duration::from_millis(20), req)
                        .await
                        .is_err()
                    {
                        Counters::bump(&n.cancelled);
                    }
                }
            }
        }
    }
}

#[test]
#[ignore = "runs for GIONG_SOAK_SECS seconds; see the module documentation"]
fn h3_soak() {
    let secs: u64 = std::env::var("GIONG_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let fds_before = open_fds();
    giong::Builder::new()
        .workers(1)
        .recv_buffers(256, giong::net::GRO_RECV_BUFFER)
        .block_on(async move {
            let (server_tls, client_tls) = tls();
            let endpoint = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(server_tls).unwrap().early_data(true),
            )
            .unwrap();
            let addr: SocketAddr = endpoint.local_addr().unwrap();
            let url = Rc::new(format!("https://{addr}"));
            drop(giong::spawn(async move {
                Server::new().serve_h3(endpoint, App).await
            }));
            let quic = ClientConfig::new(client_tls).unwrap();
            let early = quic.clone().early_data(true);

            let start = Instant::now();
            let until = start + Duration::from_secs(secs);
            let n = Rc::new(Counters::default());
            let tasks: Vec<_> = (0..CLIENTS)
                .map(|i| {
                    giong::spawn(client_task(
                        url.clone(),
                        quic.clone(),
                        early.clone(),
                        until,
                        0x9e37_79b9_7f4a_7c15 ^ i as u64,
                        n.clone(),
                    ))
                })
                .collect();

            // Memory after a warm-up of a fifth of the run, and at the end.
            let warm = start + Duration::from_secs(secs / 5);
            let mut rss_warm = 0;
            let mut fds_warm = 0;
            let mut next_report = start;
            while Instant::now() < until {
                giong::time::sleep(Duration::from_secs(1)).await;
                if rss_warm == 0 && Instant::now() >= warm {
                    rss_warm = rss_kib();
                    fds_warm = open_fds();
                }
                if Instant::now() >= next_report {
                    next_report += Duration::from_secs(60);
                    eprintln!(
                        "{:>5} s: {} connections, {} requests, {} unexpected, rss {} KiB, {} fds",
                        start.elapsed().as_secs(),
                        n.connections.get(),
                        n.requests.get(),
                        n.unexpected.get(),
                        rss_kib(),
                        open_fds(),
                    );
                }
            }
            for t in tasks {
                giong::time::timeout(Duration::from_secs(60), t)
                    .await
                    .expect("a client task never ends")
                    .unwrap();
            }
            // Let the closed connections drain.
            giong::time::sleep(Duration::from_secs(2)).await;
            let rss_end = rss_kib();
            let fds_end = open_fds();
            eprintln!(
                "done: {} connections, {} requests ({} with a body on a resumed connection, \
                 {} given up), {} unexpected; rss {rss_warm} KiB after warm-up, {rss_end} KiB \
                 at the end; fds {fds_warm} then {fds_end}",
                n.connections.get(),
                n.requests.get(),
                n.early_tried.get(),
                n.cancelled.get(),
                n.unexpected.get(),
            );
            assert_eq!(n.unexpected.get(), 0, "requests failed");
            assert!(
                fds_end <= fds_warm,
                "descriptors grew: {fds_warm} then {fds_end}"
            );
            assert!(
                rss_end <= rss_warm + rss_warm / 4 + 8 * 1024,
                "memory grew: {rss_warm} KiB then {rss_end} KiB"
            );
        })
        .unwrap();
    let fds_after = open_fds();
    assert!(
        fds_after <= fds_before + 2,
        "descriptors left after the runtime: {fds_before} then {fds_after}"
    );
}
