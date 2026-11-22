//! HTTP/3 over a path that loses, reorders and duplicates datagrams:
//! `Server::serve_h3` and giong's own client through a relay that does so
//! at set rates. Every request must come back whole, in time.
#![cfg(all(feature = "client", feature = "unstable-http3"))]

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + i / 253) as u8).collect()
}

struct App;

impl Handler for App {
    async fn call(&self, req: Request) -> Response {
        match req.path() {
            "/echo" => Response::new(200).body(req.body().to_vec()),
            path => match path.strip_prefix("/bytes/").and_then(|n| n.parse().ok()) {
                Some(n) => Response::new(200).body(pattern(n)),
                None => Response::new(404),
            },
        }
    }
}

/// How the relay treats each datagram, in percent.
#[derive(Clone, Copy)]
struct Path {
    drop: u64,
    reorder: u64,
    duplicate: u64,
}

/// xorshift: the same losses on every run.
struct Rng(u64);

impl Rng {
    fn percent(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % 100
    }
}

/// Forwards from `from` to wherever `to` gives, dropping, holding back
/// (sent after the next datagram) and duplicating as `path` says.
fn forward(
    from: Arc<UdpSocket>,
    out: Arc<UdpSocket>,
    to: impl Fn(SocketAddr) -> Option<SocketAddr> + Send + 'static,
    path: Path,
    seed: u64,
) {
    std::thread::spawn(move || {
        let mut rng = Rng(seed);
        let mut held: Option<(Vec<u8>, SocketAddr)> = None;
        let mut buf = [0u8; 65536];
        while let Ok((n, peer)) = from.recv_from(&mut buf) {
            let Some(dest) = to(peer) else { continue };
            if rng.percent() < path.drop {
                continue;
            }
            let datagram = buf[..n].to_vec();
            if held.is_none() && rng.percent() < path.reorder {
                held = Some((datagram, dest));
                continue;
            }
            let _ = out.send_to(&datagram, dest);
            if rng.percent() < path.duplicate {
                let _ = out.send_to(&datagram, dest);
            }
            if let Some((d, to)) = held.take() {
                let _ = out.send_to(&d, to);
            }
        }
    });
}

/// A relay to `server`; the address to connect to.
fn relay(server: SocketAddr, path: Path) -> SocketAddr {
    let front = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    let back = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
    let addr = front.local_addr().unwrap();
    let client = Arc::new(Mutex::new(None));
    let seen = client.clone();
    forward(
        front.clone(),
        back.clone(),
        move |peer| {
            *seen.lock().unwrap() = Some(peer);
            Some(server)
        },
        path,
        0x2545_f491_4f6c_dd1d,
    );
    forward(
        back,
        front,
        move |_| *client.lock().unwrap(),
        path,
        0x9e37_79b9_7f4a_7c15,
    );
    addr
}

fn requests_over(path: Path, sizes: &[usize], within: Duration) {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async move {
            let (server_tls, client_tls) = tls();
            let endpoint = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(server_tls).unwrap(),
            )
            .unwrap();
            let addr = relay(endpoint.local_addr().unwrap(), path);
            drop(giong::spawn(async move {
                Server::new().serve_h3(endpoint, App).await
            }));
            let client = Client::builder()
                .http3_prior_knowledge(ClientConfig::new(client_tls).unwrap())
                .connect_timeout(Duration::from_secs(10))
                .timeout(within)
                .build();
            let url = format!("https://{addr}");
            // All at once, on one connection, both ways.
            let tasks: Vec<_> = sizes
                .iter()
                .enumerate()
                .map(|(i, &n)| {
                    let (client, url) = (client.clone(), url.clone());
                    giong::spawn(async move {
                        let res = if i % 2 == 0 {
                            client.get(&format!("{url}/bytes/{n}")).send().await
                        } else {
                            client
                                .post(&format!("{url}/echo"))
                                .body(pattern(n))
                                .send()
                                .await
                        };
                        let res = res.unwrap_or_else(|e| panic!("request {i} ({n} bytes): {e}"));
                        assert_eq!(res.status(), 200);
                        assert!(res.body() == &pattern(n)[..], "request {i}: a body changed");
                    })
                })
                .collect();
            for t in tasks {
                t.await.unwrap();
            }
        })
        .unwrap();
}

const SIZES: &[usize] = &[
    0,
    1,
    1200,
    5_000,
    70_000,
    300_000,
    1 << 20,
    10,
    2_000_000,
    64,
];

#[test]
fn requests_survive_some_loss_reordering_and_duplicates() {
    let path = Path {
        drop: 3,
        reorder: 5,
        duplicate: 3,
    };
    requests_over(path, SIZES, Duration::from_secs(60));
}

#[test]
fn requests_survive_heavy_loss() {
    let path = Path {
        drop: 20,
        reorder: 10,
        duplicate: 5,
    };
    requests_over(path, &SIZES[..6], Duration::from_secs(90));
}
