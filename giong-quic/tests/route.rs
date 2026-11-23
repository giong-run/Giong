//! A server with one endpoint per worker, and a client whose address
//! changes: the kernel hands its packets to whichever worker the new
//! address hashes to, and that worker passes them on to the one that owns
//! the connection.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use giong_quic::{ClientConfig, Endpoint, ServerConfig};

const WORKERS: usize = 4;
/// Addresses the client is seen at, one after another. With four workers,
/// the chance that they all hash to the worker of the first is 4^-11.
const ADDRESSES: usize = 12;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn tls() -> (rustls::ServerConfig, rustls::ClientConfig) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut server = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key.into())
        .unwrap();
    server.alpn_protocols = vec![b"echo".to_vec()];
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client.alpn_protocols = vec![b"echo".to_vec()];
    (server, client)
}

/// What a NAT that forgets its bindings does: datagrams sent to `inside`
/// leave for `server` from the socket `current` names, and what comes back
/// on any of the sockets goes to the client.
struct Nat {
    inside: SocketAddr,
    current: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Nat {
    fn start(server: SocketAddr) -> Nat {
        let listen = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        let inside = listen.local_addr().unwrap();
        let current = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let client = Arc::new(Mutex::new(None::<SocketAddr>));
        let outside: Vec<Arc<UdpSocket>> = (0..ADDRESSES)
            .map(|_| Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap()))
            .collect();
        let mut threads = Vec::new();
        let wait = Some(Duration::from_millis(20));
        listen.set_read_timeout(wait).unwrap();
        {
            let (listen, outside, current, stop, client) = (
                listen.clone(),
                outside.clone(),
                current.clone(),
                stop.clone(),
                client.clone(),
            );
            threads.push(std::thread::spawn(move || {
                let mut buf = vec![0; 65_536];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, from)) = listen.recv_from(&mut buf) {
                        *client.lock().unwrap() = Some(from);
                        let out = &outside[current.load(Ordering::Relaxed)];
                        let _ = out.send_to(&buf[..n], server);
                    }
                }
            }));
        }
        for out in outside {
            out.set_read_timeout(wait).unwrap();
            let (listen, stop, client) = (listen.clone(), stop.clone(), client.clone());
            threads.push(std::thread::spawn(move || {
                let mut buf = vec![0; 65_536];
                while !stop.load(Ordering::Relaxed) {
                    if let Ok((n, _)) = out.recv_from(&mut buf) {
                        if let Some(client) = *client.lock().unwrap() {
                            let _ = listen.send_to(&buf[..n], client);
                        }
                    }
                }
            }));
        }
        Nat {
            inside,
            current,
            stop,
            threads,
        }
    }
}

impl Drop for Nat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn a_client_that_changes_address_keeps_its_connection() {
    let (server_tls, client_tls) = tls();
    // A port for every worker's endpoint.
    let port = UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let bound = Arc::new(AtomicUsize::new(0));
    // The workers that served a stream.
    let served = Arc::new(Mutex::new(std::collections::HashSet::new()));

    let rt = giong::Builder::new()
        .workers(WORKERS)
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .build()
        .unwrap();
    let handle = rt.handle();
    let (b, s) = (bound.clone(), served.clone());
    let server = std::thread::spawn(move || {
        rt.run(move |_| {
            let (bound, served, tls) = (b.clone(), s.clone(), server_tls.clone());
            async move {
                let endpoint = Endpoint::server(addr, ServerConfig::new(tls).unwrap()).unwrap();
                bound.fetch_add(1, Ordering::SeqCst);
                while let Some(Some(conn)) = giong::until_shutdown(endpoint.accept()).await {
                    let served = served.clone();
                    giong::spawn(async move {
                        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                            let Ok(mut req) = recv.read_to_end(4 << 20).await else {
                                return;
                            };
                            served.lock().unwrap().insert(giong::worker_id());
                            req.reverse();
                            if send.write_all(&req).await.is_err() {
                                return;
                            }
                            let _ = send.finish();
                        }
                    });
                }
            }
        })
        .unwrap();
    });
    while bound.load(Ordering::SeqCst) < WORKERS {
        std::thread::sleep(Duration::from_millis(1));
    }

    let nat = Nat::start(addr);
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls).unwrap();
            let conn = client.connect(nat.inside, "localhost", &cfg).await.unwrap();
            for address in 0..ADDRESSES {
                // From here on the server sees the client somewhere else.
                nat.current.store(address, Ordering::Relaxed);
                for n in [10usize, 30_000] {
                    let exchange = async {
                        let (mut send, mut recv) = conn.open_bi().await?;
                        let data = payload(n + address);
                        send.write_all(&data).await?;
                        send.finish()?;
                        let got = recv.read_to_end(4 << 20).await?;
                        std::io::Result::Ok((data, got))
                    };
                    let (mut want, got) = giong::time::timeout(Duration::from_secs(20), exchange)
                        .await
                        .unwrap_or_else(|_| panic!("address {address}: no answer"))
                        .unwrap_or_else(|e| panic!("address {address}: {e}"));
                    want.reverse();
                    assert!(got == want, "address {address}, {n} bytes");
                }
            }
            conn.close(0, b"");
        })
        .unwrap();
    drop(nat);
    handle.shutdown();
    server.join().unwrap();
    // One connection: one worker had it from the first packet to the last.
    assert_eq!(served.lock().unwrap().len(), 1);
}
