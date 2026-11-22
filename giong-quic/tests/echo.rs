//! giong-quic end to end: its own client against its own server, and
//! quinn (on Tokio, another thread) against a giong-quic server.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use giong_quic::{ClientConfig, Endpoint, ServerConfig, TransportConfig};

const ALPN: &[u8] = b"giong-test";

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn server_tls() -> (
    rustls::ServerConfig,
    rustls::pki_types::CertificateDer<'static>,
) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let der = cert.cert.der().clone();
    let key = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![der.clone()], key.into())
        .unwrap();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    (tls, der)
}

fn client_tls(ca: rustls::pki_types::CertificateDer<'static>) -> rustls::ClientConfig {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(ca).unwrap();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    tls
}

/// Serves: every stream is answered with its own bytes reversed.
async fn serve(ep: Endpoint) {
    while let Some(conn) = ep.accept().await {
        giong::spawn(async move {
            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                giong::spawn(async move {
                    let mut req = recv.read_to_end(4 << 20).await.unwrap();
                    req.reverse();
                    send.write_all(&req).await.unwrap();
                    send.finish().unwrap();
                });
            }
        });
    }
}

fn payload(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 + i / 251) as u8).collect()
}

#[test]
fn giong_client_and_server() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(serve(server));

            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            assert_eq!(conn.remote_addr(), addr);
            // Small and large streams, several at once.
            let mut tasks = Vec::new();
            let conn = std::rc::Rc::new(conn);
            for n in [0usize, 1, 1200, 70_000, 3 << 20] {
                let conn = conn.clone();
                tasks.push(giong::spawn(async move {
                    let (mut send, mut recv) = conn.open_bi().await.unwrap();
                    let data = payload(n);
                    send.write_all(&data).await.unwrap();
                    send.finish().unwrap();
                    let got = recv.read_to_end(4 << 20).await.unwrap();
                    let mut want = data;
                    want.reverse();
                    assert!(got == want, "stream of {n} bytes");
                }));
            }
            for t in tasks {
                giong::time::timeout(Duration::from_secs(20), t)
                    .await
                    .unwrap()
                    .unwrap();
            }
            // Five streams and more than 3 MiB each way went through.
            let stats = conn.stats();
            assert!(stats.sent_bytes > 3 << 20 && stats.received_bytes > 3 << 20);
            assert!(stats.sent_datagrams > 2000 && stats.received_datagrams > 2000);
            assert!(stats.sent_packets >= stats.sent_datagrams, "{stats:?}");
            assert!(stats.lost_packets < stats.sent_packets / 2, "{stats:?}");
            assert!(stats.rtt > Duration::ZERO && stats.rtt < Duration::from_secs(1));
            assert!(
                stats.congestion_window >= 1200 && stats.mtu >= 1200,
                "{stats:?}"
            );
        })
        .unwrap();
}

#[test]
fn a_wrong_server_name_fails_with_its_code() {
    giong::Builder::new()
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(serve(server));
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let e = match client.connect(addr, "example.org", &cfg).await {
                Ok(_) => panic!("connected with a certificate for another name"),
                Err(e) => e,
            };
            assert_eq!(
                giong::error::code(&e),
                Some(giong::error::Code::QuicConnect),
                "{e}"
            );
        })
        .unwrap();
}

#[test]
fn a_config_without_alpn_is_refused() {
    let (mut tls, _) = server_tls();
    tls.alpn_protocols.clear();
    let e = ServerConfig::new(tls).err().unwrap();
    assert_eq!(
        giong::error::code(&e),
        Some(giong::error::Code::QuicConfig),
        "{e}"
    );
}

/// quinn on Tokio (its own thread) as the client of a giong-quic server.
#[test]
fn quinn_client_against_giong_server() {
    let (tls, ca) = server_tls();
    let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
    let server = std::thread::spawn(move || {
        giong::Builder::new()
            .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
            .block_on(async move {
                let ep = Endpoint::server(
                    "127.0.0.1:0".parse().unwrap(),
                    ServerConfig::new(tls).unwrap(),
                )
                .unwrap();
                tx.send(ep.local_addr().unwrap()).unwrap();
                let _ = giong::time::timeout(Duration::from_secs(30), serve(ep)).await;
            })
            .unwrap();
    });
    let addr = rx.recv().unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_tls(ca)).unwrap();
        let mut ep = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
        ep.set_default_client_config(quinn::ClientConfig::new(Arc::new(crypto)));
        let conn = ep.connect(addr, "localhost").unwrap().await.unwrap();
        for n in [5usize, 100_000, 1 << 20] {
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            let data = payload(n);
            send.write_all(&data).await.unwrap();
            send.finish().unwrap();
            let got = recv.read_to_end(4 << 20).await.unwrap();
            let mut want = data;
            want.reverse();
            assert!(got == want, "stream of {n} bytes");
        }
        conn.close(0u32.into(), b"done");
        ep.wait_idle().await;
    });
    drop(server);
}

/// Unidirectional streams both ways, a stream the sender resets and one the
/// receiver stops: each side sees the other's code as `GIONG-E3604`.
#[test]
fn unidirectional_streams_resets_and_stops() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            let served = giong::spawn(async move {
                let conn = server.accept().await.unwrap();
                // A one-way stream in, answered on a one-way stream out.
                let mut recv = conn.accept_uni().await.unwrap();
                assert_eq!(recv.id() & 3, 2, "client-initiated, unidirectional");
                let got = recv.read_to_end(1 << 20).await.unwrap();
                let mut send = conn.open_uni().await.unwrap();
                assert_eq!(send.id() & 3, 3, "server-initiated, unidirectional");
                send.write_all(&got).await.unwrap();
                send.write_all(b"!").await.unwrap();
                send.finish().unwrap();
                // The client resets the next one after its first bytes.
                let mut recv = conn.accept_uni().await.unwrap();
                let err = recv.read_to_end(1 << 20).await.unwrap_err();
                assert_eq!(
                    giong::error::code(&err),
                    Some(giong::error::Code::QuicStreamReset)
                );
                assert!(err.to_string().contains("code 77"), "{err}");
                // And we stop reading the one after: its writer learns why.
                let mut recv = conn.accept_uni().await.unwrap();
                let first = recv.read().await.unwrap().expect("some bytes");
                assert!(first.starts_with(b"start"));
                recv.stop(99);
                // Keep the connection until the client is done.
                let _ = conn.accept_uni().await;
            });

            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            let mut send = conn.open_uni().await.unwrap();
            send.write_all(b"one way").await.unwrap();
            send.finish().unwrap();
            let mut recv = conn.accept_uni().await.unwrap();
            assert_eq!(recv.read_to_end(100).await.unwrap(), b"one way!");

            let mut send = conn.open_uni().await.unwrap();
            send.write_all(b"never finished").await.unwrap();
            send.reset(77);

            let mut send = conn.open_uni().await.unwrap();
            send.write_all(b"start").await.unwrap();
            let chunk = vec![0u8; 64 * 1024];
            let err = giong::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Err(e) = send.write_all(&chunk).await {
                        break e;
                    }
                }
            })
            .await
            .expect("the writer is told to stop");
            assert_eq!(
                giong::error::code(&err),
                Some(giong::error::Code::QuicStreamReset)
            );
            assert!(err.to_string().contains("code 99"), "{err}");
            drop(conn);
            let _ = giong::time::timeout(Duration::from_secs(5), served).await;
        })
        .unwrap();
}

/// A `connect` whose future is dropped (here by a timeout: nothing listens
/// on the port) closes its connection instead of leaving it behind.
#[test]
fn a_dropped_connect_leaves_no_connection() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            // A bound socket that never answers.
            let silent = giong::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let to = silent.local_addr().unwrap();
            let (_, ca) = server_tls();
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let attempt = client.connect(to, "localhost", &cfg);
            assert!(giong::time::timeout(Duration::from_millis(100), attempt)
                .await
                .is_err());
            // Closed at once; forgotten when its draining period is over
            // (three probe timeouts, about three seconds before any RTT
            // sample).
            let gone = giong::time::timeout(Duration::from_secs(20), async {
                while client.connections() > 0 {
                    giong::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await;
            assert!(gone.is_ok(), "{} connections left", client.connections());
        })
        .unwrap();
}

/// quinn-proto reuses the name of a connection once it has drained, which
/// can be while streams of it are still held.
#[test]
fn streams_held_past_their_connection_do_not_touch_the_next_one() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(serve(server));
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();

            let old = client.connect(addr, "localhost", &cfg).await.unwrap();
            let held = old.open_bi().await.unwrap();
            old.close(0, b"");
            drop(old);
            // Long enough for the closed connection to drain.
            giong::time::sleep(Duration::from_millis(500)).await;

            let new = client.connect(addr, "localhost", &cfg).await.unwrap();
            drop(held);
            let (mut send, mut recv) = new.open_bi().await.unwrap();
            send.write_all(b"abc").await.unwrap();
            send.finish().unwrap();
            assert_eq!(recv.read_to_end(16).await.unwrap(), b"cba");
        })
        .unwrap();
}

/// quinn-proto reports no event for a close of our own, and tasks may
/// wait on the connection when it is closed.
#[test]
fn closing_a_connection_ends_what_waits_on_it() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(serve(server));
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = std::rc::Rc::new(client.connect(addr, "localhost", &cfg).await.unwrap());

            // A stream the server answers only once it is finished, which
            // it never is: the read waits.
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            send.write_all(b"abc").await.unwrap();
            let reader = giong::spawn(async move { recv.read().await });
            // The server opens no stream of its own: this waits too.
            let accepting = conn.clone();
            let acceptor = giong::spawn(async move { accepting.accept_uni().await.map(|_| ()) });
            giong::time::sleep(Duration::from_millis(50)).await;

            conn.close(7, b"done");
            let read = giong::time::timeout(Duration::from_secs(5), reader)
                .await
                .expect("the read never ends")
                .unwrap();
            assert!(read.unwrap_err().to_string().contains("GIONG-E3603"));
            let accepted = giong::time::timeout(Duration::from_secs(5), acceptor)
                .await
                .expect("the accept never ends")
                .unwrap();
            assert!(accepted.unwrap_err().to_string().contains("GIONG-E3603"));
            assert!(conn.open_bi().await.is_err());
            drop(send);
        })
        .unwrap();
}

/// A relay between a client and `server` over loopback that drops what
/// the client sends once `cut` is set. Returns the address to connect to.
fn relay(server: SocketAddr, cut: Arc<AtomicBool>) -> SocketAddr {
    let front = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
    let back = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
    let addr = front.local_addr().unwrap();
    let client = Arc::new(std::sync::Mutex::new(None));
    let (f, b, c) = (front.clone(), back.clone(), client.clone());
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while let Ok((n, from)) = f.recv_from(&mut buf) {
            *c.lock().unwrap() = Some(from);
            if !cut.load(Ordering::Relaxed) {
                let _ = b.send_to(&buf[..n], server);
            }
        }
    });
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while let Ok((n, _)) = back.recv_from(&mut buf) {
            if let Some(to) = *client.lock().unwrap() {
                let _ = front.send_to(&buf[..n], to);
            }
        }
    });
    addr
}

/// The server answers with more than its congestion window, whose ACKs
/// never come, and closes the connection while the rest of the answer
/// waits to be sent: with `Connection::close`, or by dropping the last
/// handle. The client must hear of the close well before its idle timeout.
fn close_while_data_waits(by_drop: bool) {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async move {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let cut = Arc::new(AtomicBool::new(false));
            let addr = relay(server.local_addr().unwrap(), cut.clone());
            giong::spawn(async move {
                let conn = server.accept().await.unwrap();
                let (mut send, recv) = conn.accept_bi().await.unwrap();
                cut.store(true, Ordering::Relaxed);
                // Hands QUIC the answer (or what flow control takes of it).
                let _ = giong::time::timeout(
                    Duration::from_millis(50),
                    send.write_all(&payload(1 << 20)),
                )
                .await;
                // Long enough for a window of it to go out unacknowledged.
                giong::time::sleep(Duration::from_millis(50)).await;
                if by_drop {
                    drop((send, recv, conn));
                } else {
                    conn.close(9, b"enough");
                }
                giong::time::sleep(Duration::from_secs(10)).await;
            });
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            send.write_all(b"go").await.unwrap();
            // The idle timeout is 30 s: an answer within 3 s is the close.
            let read = giong::time::timeout(Duration::from_secs(3), recv.read_to_end(8 << 20))
                .await
                .expect("the close never reached the client");
            let e = read.unwrap_err().to_string();
            let expected = if by_drop {
                "closed by peer"
            } else {
                "closed by peer: enough (code 9)"
            };
            assert!(e.contains(expected), "{e}");
        })
        .unwrap();
}

#[test]
fn a_close_reaches_the_peer_while_data_waits_to_be_sent() {
    close_while_data_waits(false);
}

#[test]
fn dropping_the_last_handle_closes_while_data_waits_to_be_sent() {
    close_while_data_waits(true);
}

/// A server endpoint that answers every stream with its bytes reversed,
/// and notes for each stream whether it came in early data.
fn early_server(
    tls: rustls::ServerConfig,
    early: bool,
) -> (SocketAddr, std::rc::Rc<std::cell::RefCell<Vec<bool>>>) {
    let config = ServerConfig::new(tls).unwrap().early_data(early);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), config).unwrap();
    let addr = server.local_addr().unwrap();
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let noted = seen.clone();
    giong::spawn(async move {
        while let Some(conn) = server.accept().await {
            let noted = noted.clone();
            giong::spawn(async move {
                while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                    noted.borrow_mut().push(recv.is_early());
                    let Ok(mut req) = recv.read_to_end(1 << 20).await else {
                        return;
                    };
                    req.reverse();
                    let _ = send.write_all(&req).await;
                    let _ = send.finish();
                }
            });
        }
    });
    (addr, seen)
}

async fn echo_on(conn: &giong_quic::Connection, data: &[u8]) -> std::io::Result<Vec<u8>> {
    let (mut send, mut recv) = conn.open_bi().await?;
    send.write_all(data).await?;
    send.finish()?;
    recv.read_to_end(1 << 20).await
}

#[test]
fn a_resumed_client_sends_its_first_stream_in_early_data() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let (addr, seen) = early_server(tls, true);
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap().early_data(true);

            // A first connection leaves a ticket behind.
            let conn = client.connect_early(addr, "localhost", &cfg).await.unwrap();
            assert_eq!(echo_on(&conn, b"one").await.unwrap(), b"eno");
            assert!(!conn.handshake().await.unwrap());
            conn.close(0, b"");
            drop(conn);

            // The second one sends before its handshake is over.
            let conn = client.connect_early(addr, "localhost", &cfg).await.unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            assert!(recv.is_early());
            send.write_all(b"early").await.unwrap();
            send.finish().unwrap();
            assert_eq!(recv.read_to_end(64).await.unwrap(), b"ylrae");
            assert!(conn.handshake().await.unwrap());
            // After the handshake, streams are not early any more.
            let (mut send, recv) = conn.open_bi().await.unwrap();
            assert!(!recv.is_early());
            // Ended empty (dropping it unfinished would reset it).
            send.finish().unwrap();
            drop((send, recv));
            assert_eq!(echo_on(&conn, b"late").await.unwrap(), b"etal");
            assert_eq!(*seen.borrow(), [false, true, false, false]);
        })
        .unwrap();
}

#[test]
fn early_data_the_server_refuses_fails_its_streams_and_not_the_connection() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            // Two servers with the same certificate but their own session
            // caches (a clone of a rustls configuration shares its cache):
            // a ticket of the first means nothing to the second.
            let mut other = tls.clone();
            other.session_storage = rustls::server::ServerSessionMemoryCache::new(256);
            let (first, _) = early_server(tls, true);
            let (second, seen) = early_server(other, true);
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap().early_data(true);
            let conn = client.connect(first, "localhost", &cfg).await.unwrap();
            assert_eq!(echo_on(&conn, b"one").await.unwrap(), b"eno");
            conn.close(0, b"");
            drop(conn);

            let conn = client
                .connect_early(second, "localhost", &cfg)
                .await
                .unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            assert!(recv.is_early());
            let _ = send.write_all(b"early").await;
            let _ = send.finish();
            let read = giong::time::timeout(Duration::from_secs(5), recv.read_to_end(64))
                .await
                .expect("the refused stream never ends");
            let error = read.unwrap_err();
            assert_eq!(
                giong::error::code(&error),
                Some(giong::error::Code::QuicEarlyDataRejected),
                "{error}"
            );
            assert!(send.write_all(b"more").await.is_err());
            assert!(!conn.handshake().await.unwrap());
            drop((send, recv));
            // The connection works, with streams numbered from 0 again.
            assert_eq!(echo_on(&conn, b"again").await.unwrap(), b"niaga");
            assert_eq!(*seen.borrow(), [false]);
        })
        .unwrap();
}

#[test]
fn a_server_without_early_data_waits_for_the_handshake() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let (addr, seen) = early_server(tls, false);
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap().early_data(true);
            for round in 0..2 {
                let conn = client.connect_early(addr, "localhost", &cfg).await.unwrap();
                let (mut send, recv) = conn.open_bi().await.unwrap();
                // No ticket that allows early data: connect_early waited.
                assert!(!recv.is_early(), "round {round}");
                send.finish().unwrap();
                drop((send, recv));
                assert_eq!(echo_on(&conn, b"x").await.unwrap(), b"x");
                conn.close(0, b"");
            }
            assert_eq!(*seen.borrow(), [false; 4]);
        })
        .unwrap();
}

#[test]
fn a_server_that_does_not_accept_refuses_connections_past_its_bound() {
    giong::Builder::new()
        .workers(1)
        .recv_buffers(512, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            // No connection may time out while the waves run: one that did
            // would free its place, and a later one would take it. Under
            // ThreadSanitizer the waves outlast the default 30 seconds.
            let patient = || TransportConfig::default().idle_timeout(Duration::from_secs(600));
            // Nobody accepts on this endpoint.
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap().transport(patient()),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            let client =
                std::rc::Rc::new(Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap());
            let cfg = ClientConfig::new(client_tls(ca))
                .unwrap()
                .transport(patient());
            let (mut made, mut refused) = (Vec::new(), 0);
            // In waves, so that the client's own socket keeps up.
            for _ in 0..12 {
                let tasks: Vec<_> = (0..100)
                    .map(|_| {
                        let (client, cfg) = (client.clone(), cfg.clone());
                        giong::spawn(async move {
                            giong::time::timeout(
                                Duration::from_secs(10),
                                client.connect(addr, "localhost", &cfg),
                            )
                            .await
                        })
                    })
                    .collect();
                for t in tasks {
                    match t.await.unwrap().expect("a connect never ends") {
                        Ok(conn) => made.push(conn),
                        Err(e) => {
                            assert!(e.to_string().contains("refused"), "{e}");
                            refused += 1;
                        }
                    }
                }
            }
            assert_eq!((made.len(), refused), (1024, 176));
            drop(server);
        })
        .unwrap();
}

/// The first Initial datagram of a QUIC client that will never answer: what
/// a client with a forged source address sends.
fn forged_initial(ca: &rustls::pki_types::CertificateDer<'static>, to: SocketAddr) -> Vec<u8> {
    let tls = Arc::new(client_tls(ca.clone()));
    let crypto = quinn_proto::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
    let cfg = quinn_proto::ClientConfig::new(Arc::new(crypto));
    let mut ep = quinn_proto::Endpoint::new(Default::default(), None, true, None);
    let now = std::time::Instant::now();
    let (_, mut conn) = ep.connect(now, cfg, to, "localhost").unwrap();
    let mut buf = Vec::new();
    let t = conn.poll_transmit(now, 1, &mut buf).unwrap();
    buf.truncate(t.size);
    buf
}

#[test]
fn a_flood_of_forged_initials_does_not_lock_real_clients_out() {
    giong::Builder::new()
        .workers(1)
        .recv_buffers(512, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(serve(server));
            // More forged handshakes than the endpoint holds, each from an
            // address of its own that never reads what comes back.
            let forged: Vec<_> = (0..1200).map(|_| forged_initial(&ca, addr)).collect();
            let mut sockets = Vec::new();
            for (i, datagram) in forged.iter().enumerate() {
                let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                sock.send_to(datagram, addr).unwrap();
                sockets.push(sock);
                if i % 50 == 0 {
                    giong::time::sleep(Duration::from_millis(2)).await;
                }
            }
            giong::time::sleep(Duration::from_millis(100)).await;
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = giong::time::timeout(
                Duration::from_secs(5),
                client.connect(addr, "localhost", &cfg),
            )
            .await
            .expect("the real client got no answer")
            .expect("the real client was locked out");
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            send.write_all(b"abc").await.unwrap();
            send.finish().unwrap();
            assert_eq!(recv.read_to_end(10).await.unwrap(), b"cba");
        })
        .unwrap();
}

#[test]
fn connections_not_accepted_are_closed_with_their_endpoint() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            // Connected, and waiting for an accept that never comes.
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            drop(server);
            // The idle timeout is 30 s: an end within 3 s is the close.
            let ended = giong::time::timeout(Duration::from_secs(3), conn.accept_uni())
                .await
                .expect("the connection outlived its endpoint");
            let e = ended.err().unwrap().to_string();
            assert!(e.contains("closed by peer"), "{e}");
        })
        .unwrap();
}

#[test]
fn a_writer_waiting_for_credit_is_woken_after_its_reading_half_is_dropped() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            // The server reads only after a while, so the client's writer
            // waits for flow control credit meanwhile.
            giong::spawn(async move {
                let conn = server.accept().await.unwrap();
                let (_send, mut recv) = conn.accept_bi().await.unwrap();
                giong::time::sleep(Duration::from_millis(200)).await;
                let got = recv.read_to_end(16 << 20).await.unwrap();
                assert_eq!(got.len(), 8 << 20);
                giong::time::sleep(Duration::from_secs(5)).await;
            });
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            let (mut send, recv) = conn.open_bi().await.unwrap();
            let writer = giong::spawn(async move {
                send.write_all(&payload(8 << 20)).await.unwrap();
                send.finish().unwrap();
                send
            });
            // The reading half goes while the writer is blocked.
            giong::time::sleep(Duration::from_millis(50)).await;
            drop(recv);
            giong::time::timeout(Duration::from_secs(10), writer)
                .await
                .expect("the writer was never woken")
                .unwrap();
        })
        .unwrap();
}

// Needs a second address of this machine: macOS has 127.0.0.1 only,
// unless more are added to the interface.
#[test]
#[cfg(target_os = "linux")]
fn a_wildcard_server_answers_from_the_address_it_was_sent_to() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "0.0.0.0:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let port = server.local_addr().unwrap().port();
            giong::spawn(serve(server));
            let client = Endpoint::client("0.0.0.0:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            // 127.0.0.2 is this machine too, but the route to the client
            // would answer from 127.0.0.1, which the client drops.
            let addr: SocketAddr = format!("127.0.0.2:{port}").parse().unwrap();
            let conn = giong::time::timeout(
                Duration::from_secs(5),
                client.connect(addr, "localhost", &cfg),
            )
            .await
            .expect("no answer from the address connected to")
            .unwrap();
            let (mut send, mut recv) = conn.open_bi().await.unwrap();
            send.write_all(b"abc").await.unwrap();
            send.finish().unwrap();
            assert_eq!(recv.read_to_end(64).await.unwrap(), b"cba");
        })
        .unwrap();
}

#[test]
fn streams_a_server_opens_are_never_early() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let config = ServerConfig::new(tls).unwrap().early_data(true);
            let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), config).unwrap();
            let addr = server.local_addr().unwrap();
            let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let noted = seen.clone();
            giong::spawn(async move {
                while let Some(conn) = server.accept().await {
                    // At once, which for a resumed client is before its
                    // handshake ends: what the server sends is not 0-RTT.
                    let mut send = conn.open_uni().await.unwrap();
                    noted.borrow_mut().push(send.is_early());
                    send.write_all(b"hi").await.unwrap();
                    send.finish().unwrap();
                    giong::spawn(async move {
                        let _stream = conn.accept_bi().await;
                        giong::time::sleep(Duration::from_secs(2)).await;
                    });
                }
            });
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap().early_data(true);
            for _ in 0..2 {
                let conn = client.connect_early(addr, "localhost", &cfg).await.unwrap();
                let (mut send, _recv) = conn.open_bi().await.unwrap();
                send.write_all(b"x").await.unwrap();
                let mut uni = conn.accept_uni().await.unwrap();
                assert_eq!(uni.read_to_end(16).await.unwrap(), b"hi");
                conn.close(0, b"");
            }
            assert_eq!(*seen.borrow(), [false, false]);
        })
        .unwrap();
}

/// Two servers with the same stateless reset key, as after a restart: a
/// client of the first, whose packets now reach the second, learns at once
/// that its connection is gone (RFC 9000, section 10.3) instead of waiting
/// for its idle timeout.
#[test]
fn a_server_with_the_same_reset_key_ends_connections_it_does_not_know() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let key = [7u8; 32];
            let server = |tls: rustls::ServerConfig| {
                let config = ServerConfig::new(tls).unwrap().stateless_reset_key(key);
                let ep = Endpoint::server("127.0.0.1:0".parse().unwrap(), config).unwrap();
                let addr = ep.local_addr().unwrap();
                giong::spawn(serve(ep));
                addr
            };
            let first = server(tls.clone());
            let second = server(tls);
            // A relay whose far side can be switched from one to the other.
            let target = Arc::new(std::sync::Mutex::new(first));
            let front = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
            let back = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
            let relay_addr = front.local_addr().unwrap();
            let client_addr = Arc::new(std::sync::Mutex::new(None));
            {
                let (front, back, target, client_addr) = (
                    front.clone(),
                    back.clone(),
                    target.clone(),
                    client_addr.clone(),
                );
                std::thread::spawn(move || {
                    let mut buf = [0u8; 65536];
                    while let Ok((n, from)) = front.recv_from(&mut buf) {
                        *client_addr.lock().unwrap() = Some(from);
                        let to = *target.lock().unwrap();
                        let _ = back.send_to(&buf[..n], to);
                    }
                });
            }
            std::thread::spawn(move || {
                let mut buf = [0u8; 65536];
                while let Ok((n, _)) = back.recv_from(&mut buf) {
                    if let Some(to) = *client_addr.lock().unwrap() {
                        let _ = front.send_to(&buf[..n], to);
                    }
                }
            });
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(relay_addr, "localhost", &cfg).await.unwrap();
            assert_eq!(echo_on(&conn, b"abc").await.unwrap(), b"cba");
            // The first server is gone from the path; the second never
            // saw this connection.
            *target.lock().unwrap() = second;
            let ended = giong::time::timeout(Duration::from_secs(3), async {
                loop {
                    if echo_on(&conn, b"abc").await.is_err() {
                        break;
                    }
                }
            })
            .await;
            assert!(ended.is_ok(), "the client waited for its idle timeout");
        })
        .unwrap();
}

/// Clients that resume with 0-RTT data and never finish their handshakes
/// (from addresses that never answer, as when a recorded flight is
/// replayed to servers that do not share one session cache): past the
/// Retry bound, those handed over early but not validated make the next
/// unvalidated clients prove their address, instead of each taking a
/// connection.
#[test]
fn early_connections_that_never_validate_are_bounded() {
    giong::Builder::new()
        .workers(1)
        .recv_buffers(512, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let config = ServerConfig::new(tls).unwrap().early_data(true);
            let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), config).unwrap();
            let addr = server.local_addr().unwrap();
            let accepted = std::rc::Rc::new(std::cell::Cell::new(0usize));
            let count = accepted.clone();
            giong::spawn(async move {
                let mut held = Vec::new();
                while let Some(conn) = server.accept().await {
                    count.set(count.get() + 1);
                    held.push(conn);
                }
            });
            let mut client_tls = client_tls(ca);
            client_tls.enable_early_data = true;
            let cfg = ClientConfig::new(client_tls.clone())
                .unwrap()
                .early_data(true);
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let crypto =
                quinn_proto::crypto::rustls::QuicClientConfig::try_from(Arc::new(client_tls))
                    .unwrap();
            let qcfg = quinn_proto::ClientConfig::new(Arc::new(crypto));
            let mut ep = quinn_proto::Endpoint::new(Default::default(), None, true, None);
            // First flights with a request in 0-RTT, one ticket each; real
            // connections fill the shared ticket store when it runs out.
            let mut flights = Vec::new();
            while flights.len() < 150 {
                let now = std::time::Instant::now();
                let (_, mut qconn) = ep.connect(now, qcfg.clone(), addr, "localhost").unwrap();
                let Some(id) = qconn.streams().open(quinn_proto::Dir::Bi) else {
                    let conn = client.connect_early(addr, "localhost", &cfg).await.unwrap();
                    conn.handshake().await.unwrap();
                    giong::time::sleep(Duration::from_millis(20)).await;
                    conn.close(0, b"");
                    continue;
                };
                qconn.send_stream(id).write(b"GET /").unwrap();
                let mut flight = Vec::new();
                loop {
                    let mut buf = Vec::new();
                    match qconn.poll_transmit(now, 1, &mut buf) {
                        Some(t) => {
                            buf.truncate(t.size);
                            flight.push(buf);
                        }
                        None => break,
                    }
                }
                flights.push(flight);
            }
            giong::time::sleep(Duration::from_millis(100)).await;
            accepted.set(0);
            // Each from an address that never answers.
            let mut sockets = Vec::new();
            for (i, flight) in flights.iter().enumerate() {
                let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                for datagram in flight {
                    sock.send_to(datagram, addr).unwrap();
                }
                sockets.push(sock);
                if i % 25 == 0 {
                    giong::time::sleep(Duration::from_millis(2)).await;
                }
            }
            giong::time::sleep(Duration::from_millis(300)).await;
            let taken = accepted.get();
            assert!(
                taken > 0,
                "none was handed over early: the test checks nothing"
            );
            assert!(
                taken <= 80,
                "{taken} unvalidated clients took a connection each"
            );
        })
        .unwrap();
}

#[test]
fn a_closed_endpoint_still_draining_refuses_new_clients_at_once() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            // One connection, held; then the endpoint goes, and the
            // connection keeps its socket open.
            giong::spawn(async move {
                let conn = server.accept().await.unwrap();
                drop(server);
                giong::time::sleep(Duration::from_secs(10)).await;
                drop(conn);
            });
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let _held = client.connect(addr, "localhost", &cfg).await.unwrap();
            giong::time::sleep(Duration::from_millis(50)).await;
            let started = std::time::Instant::now();
            let refused = giong::time::timeout(
                Duration::from_secs(5),
                client.connect(addr, "localhost", &cfg),
            )
            .await
            .expect("the closed endpoint left the client to its timeout");
            assert!(refused.is_err());
            assert!(started.elapsed() < Duration::from_secs(2));
        })
        .unwrap();
}

#[test]
fn finishing_a_stream_the_peer_stopped_gives_its_code() {
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            let (tls, ca) = server_tls();
            let server = Endpoint::server(
                "127.0.0.1:0".parse().unwrap(),
                ServerConfig::new(tls).unwrap(),
            )
            .unwrap();
            let addr = server.local_addr().unwrap();
            giong::spawn(async move {
                let conn = server.accept().await.unwrap();
                let (_send, mut recv) = conn.accept_bi().await.unwrap();
                // "I have my answer": stop sending, code 7.
                recv.stop(7);
                giong::time::sleep(Duration::from_secs(2)).await;
            });
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(addr, "localhost", &cfg).await.unwrap();
            let (mut send, _recv) = conn.open_bi().await.unwrap();
            send.write_all(b"abc").await.unwrap();
            // Something else is awaited before finishing; STOP_SENDING
            // arrives meanwhile.
            giong::time::sleep(Duration::from_millis(200)).await;
            let e = send.finish().expect_err("the peer stopped the stream");
            assert_eq!(giong_quic::reset_code(&e), Some(7), "{e}");
        })
        .unwrap();
}

/// A relay from a local address to `target`, which can be switched.
fn switchable_relay(target: Arc<std::sync::Mutex<SocketAddr>>) -> SocketAddr {
    let front = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
    let back = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
    let addr = front.local_addr().unwrap();
    let client = Arc::new(std::sync::Mutex::new(None));
    {
        let (front, back, client) = (front.clone(), back.clone(), client.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            while let Ok((n, from)) = front.recv_from(&mut buf) {
                *client.lock().unwrap() = Some(from);
                let to = *target.lock().unwrap();
                let _ = back.send_to(&buf[..n], to);
            }
        });
    }
    std::thread::spawn(move || {
        let mut buf = [0u8; 65536];
        while let Ok((n, _)) = back.recv_from(&mut buf) {
            if let Some(to) = *client.lock().unwrap() {
                let _ = front.send_to(&buf[..n], to);
            }
        }
    });
    addr
}

/// After a restart with fewer workers: a client whose connection IDs name
/// a worker that has no endpoint any more is still told at once that its
/// connection is gone (a stateless reset with the shared key).
#[test]
fn a_connection_id_of_a_worker_that_is_gone_gets_a_stateless_reset() {
    let (tls, ca) = server_tls();
    let key = [9u8; 32];
    // The new server: two workers, an endpoint on worker 1 only.
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let second: SocketAddr = ([127, 0, 0, 1], port).into();
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let new_tls = tls.clone();
    std::thread::spawn(move || {
        rt.run(move |_| {
            let tls = new_tls.clone();
            async move {
                if giong::worker_id() == 1 {
                    let config = ServerConfig::new(tls).unwrap().stateless_reset_key(key);
                    let ep = Endpoint::server(second, config).unwrap();
                    serve(ep).await;
                } else {
                    std::future::pending::<()>().await;
                }
            }
        })
        .unwrap();
    });
    std::thread::sleep(Duration::from_millis(200));
    giong::Builder::new()
        .recv_buffers(64, giong::net::GRO_RECV_BUFFER)
        .block_on(async {
            // The old server: its connection IDs name worker 0.
            let config = ServerConfig::new(tls).unwrap().stateless_reset_key(key);
            let ep = Endpoint::server("127.0.0.1:0".parse().unwrap(), config).unwrap();
            let first = ep.local_addr().unwrap();
            giong::spawn(serve(ep));
            let target = Arc::new(std::sync::Mutex::new(first));
            let relay = switchable_relay(target.clone());
            let client = Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
            let cfg = ClientConfig::new(client_tls(ca)).unwrap();
            let conn = client.connect(relay, "localhost", &cfg).await.unwrap();
            assert_eq!(echo_on(&conn, b"abc").await.unwrap(), b"cba");
            *target.lock().unwrap() = second;
            let ended = giong::time::timeout(Duration::from_secs(3), async {
                while echo_on(&conn, b"abc").await.is_ok() {}
            })
            .await;
            assert!(ended.is_ok(), "the client waited for its idle timeout");
        })
        .unwrap();
}
