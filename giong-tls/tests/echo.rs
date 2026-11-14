//! A TLS echo server on giong, driven by a blocking rustls client.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use giong::net::TcpListener;
use giong::time::timeout;
use giong_tls::rustls::pki_types::{CertificateDer, ServerName};
use giong_tls::rustls::{self, ClientConfig, ClientConnection, RootCertStore, StreamOwned};
use giong_tls::{TlsAcceptor, TlsStream};

type Client = StreamOwned<ClientConnection, TcpStream>;

struct Certs {
    acceptor: TlsAcceptor,
    client: Arc<ClientConfig>,
}

fn certs(versions: &[&'static rustls::SupportedProtocolVersion]) -> Certs {
    let key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_pem = key.cert.pem();
    let key_pem = key.key_pair.serialize_pem();
    let acceptor = TlsAcceptor::from_pem(cert_pem.as_bytes(), key_pem.as_bytes()).unwrap();

    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(key.cert.der().to_vec()))
        .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Certs {
        acceptor,
        client: Arc::new(client),
    }
}

fn connect(addr: SocketAddr, config: &Arc<ClientConfig>) -> Client {
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let name = ServerName::try_from("localhost").unwrap();
    StreamOwned::new(ClientConnection::new(config.clone(), name).unwrap(), tcp)
}

/// Runs `serve` for every accepted TLS connection until the client side of the
/// test is done.
fn with_server<F, Fut>(acceptor: TlsAcceptor, serve: F, client: impl FnOnce(SocketAddr))
where
    F: Fn(TlsStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let (tx, rx) = mpsc::channel();
    let serve = Arc::new(serve);
    let server = std::thread::spawn(move || {
        rt.run(move |_| {
            let (tx, acceptor, serve) = (tx.clone(), acceptor.clone(), serve.clone());
            async move {
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                while let Some(Ok(tcp)) = giong::until_shutdown(listener.accept()).await {
                    let (acceptor, serve) = (acceptor.clone(), serve.clone());
                    giong::spawn(async move {
                        if let Ok(tls) = acceptor.accept(tcp).await {
                            serve(tls).await;
                        }
                    });
                }
            }
        })
        .unwrap();
    });
    client(rx.recv().unwrap());
    handle.shutdown();
    server.join().unwrap();
}

async fn echo(tls: TlsStream) {
    while let Ok(data) = tls.recv().await {
        if data.is_empty() {
            break;
        }
        if tls.write_all(data).await.0.is_err() {
            return;
        }
    }
    let _ = tls.shutdown().await;
}

fn roundtrip(versions: &[&'static rustls::SupportedProtocolVersion]) {
    let c = certs(versions);
    let client_config = c.client.clone();
    with_server(c.acceptor, echo, move |addr| {
        let mut client = connect(addr, &client_config);
        client.write_all(b"hello over tls").unwrap();
        let mut buf = [0u8; 14];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello over tls");

        // Several megabytes in each direction, written while reading.
        let data: Vec<u8> = (0..4_000_000u32).map(|i| (i % 253) as u8).collect();
        let expected = data.clone();
        let mut reader = connect(addr, &client_config);
        reader.write_all(&data[..1000]).unwrap();
        let mut got = vec![0u8; 1000];
        reader.read_exact(&mut got).unwrap();
        assert_eq!(got, &expected[..1000]);
        // Interleave: send a slice, read it back, so neither side stalls.
        for piece in data[1000..].chunks(200_000) {
            reader.write_all(piece).unwrap();
            let mut back = vec![0u8; piece.len()];
            reader.read_exact(&mut back).unwrap();
            assert!(back == piece);
        }

        // A clean close: our close_notify is answered with one.
        client.conn.send_close_notify();
        client.flush().unwrap();
        while client.conn.wants_write() {
            client.conn.write_tls(&mut client.sock).unwrap();
        }
        let mut rest = Vec::new();
        client.read_to_end(&mut rest).unwrap();
        assert!(rest.is_empty());
    });
}

#[test]
fn echoes_over_tls_1_3() {
    roundtrip(&[&rustls::version::TLS13]);
}

#[test]
fn echoes_over_tls_1_2() {
    roundtrip(&[&rustls::version::TLS12]);
}

#[test]
fn pieces_written_together_arrive_in_order() {
    // A header, a body larger than rustls' send buffer, an empty piece and
    // a trailer in one vectored write.
    fn pieces() -> Vec<Vec<u8>> {
        let body: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        vec![b"head:".to_vec(), body, Vec::new(), b":tail".to_vec()]
    }
    let c = certs(&[&rustls::version::TLS13]);
    let client_config = c.client.clone();
    let serve = |tls: TlsStream| async move {
        if tls.recv().await.is_err() {
            return;
        }
        let (res, back) = tls.write_all_vectored(pieces()).await;
        if res.is_ok() && back == pieces() {
            let _ = tls.shutdown().await;
        }
    };
    with_server(c.acceptor, serve, move |addr| {
        let mut client = connect(addr, &client_config);
        client.write_all(b"go").unwrap();
        let mut got = Vec::new();
        // Fails without the server's close_notify, which it sends only if
        // the write succeeded and handed the pieces back.
        client.read_to_end(&mut got).unwrap();
        assert!(got == pieces().concat());
    });
}

#[test]
fn a_file_is_read_and_encrypted_in_the_process() {
    let data: Vec<u8> = (0..700_001u32).map(|i| (i % 249) as u8).collect();
    let path = std::env::temp_dir().join(format!("giong-tls-echo-{}-file", std::process::id()));
    std::fs::write(&path, &data).unwrap();
    let c = certs(&[&rustls::version::TLS13]);
    let client_config = c.client.clone();
    let served = path.clone();
    let serve = move |tls: TlsStream| {
        let path = served.clone();
        async move {
            assert!(!tls.is_ktls());
            if tls.recv().await.is_err() {
                return;
            }
            let file = giong::fs::File::open(&path).await.unwrap();
            let len = file.len().unwrap();
            tls.send_file(&file, 0, len).await.unwrap();
            tls.send_file(&file, 300_000, 5).await.unwrap();
            tls.send_file(&file, 0, 0).await.unwrap();
            // Past the end: what the file had, then the error; the stream
            // can still send.
            let err = tls.send_file(&file, len - 10, 20).await.unwrap_err();
            assert_eq!(
                giong::error::code(&err),
                Some(giong::error::Code::FileTooShort),
                "{err}"
            );
            assert!(err.to_string().contains("10 bytes short"), "{err}");
            tls.write_all(b"end".to_vec()).await.0.unwrap();
            let _ = tls.shutdown().await;
        }
    };
    with_server(c.acceptor, serve, move |addr| {
        let mut client = connect(addr, &client_config);
        client.write_all(b"go").unwrap();
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        let mut expected = data.clone();
        expected.extend_from_slice(&data[300_000..300_005]);
        expected.extend_from_slice(&data[data.len() - 10..]);
        expected.extend_from_slice(b"end");
        assert!(got == expected);
    });
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn alpn_is_negotiated() {
    let c = certs(rustls::ALL_VERSIONS);
    let mut config = (*c.client).clone();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let config = Arc::new(config);
    async fn report(tls: TlsStream) {
        let alpn = tls.alpn_protocol().unwrap_or_default();
        let _ = tls.write_all(alpn).await;
        let _ = tls.shutdown().await;
    }
    let acceptor = c.acceptor.alpn(&[b"http/1.1"]);
    with_server(acceptor, report, move |addr| {
        let mut client = connect(addr, &config);
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"http/1.1");
        assert_eq!(client.conn.alpn_protocol(), Some(&b"http/1.1"[..]));
    });
}

#[test]
fn from_pem_offers_no_alpn_so_any_client_protocol_is_accepted() {
    let c = certs(rustls::ALL_VERSIONS);
    let mut config = (*c.client).clone();
    // An h2-only client (gRPC, say): a server offering only http/1.1 would
    // refuse it with no_application_protocol.
    config.alpn_protocols = vec![b"h2".to_vec()];
    let config = Arc::new(config);
    async fn report(tls: TlsStream) {
        let alpn = tls.alpn_protocol().unwrap_or_else(|| b"none".to_vec());
        let _ = tls.write_all(alpn).await;
        let _ = tls.shutdown().await;
    }
    with_server(c.acceptor, report, move |addr| {
        let mut client = connect(addr, &config);
        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"none");
        assert_eq!(client.conn.alpn_protocol(), None);
    });
}

#[test]
fn recv_is_cancel_safe() {
    // The server wraps every `recv` in a timeout far shorter than the gaps
    // between the client's writes, so most of them are cancelled.
    async fn impatient(tls: TlsStream) {
        loop {
            match timeout(Duration::from_millis(2), tls.recv()).await {
                Err(_) => continue,
                Ok(Ok(data)) if !data.is_empty() => {
                    if tls.write_all(data).await.0.is_err() {
                        return;
                    }
                }
                Ok(_) => break,
            }
        }
        let _ = tls.shutdown().await;
    }
    let c = certs(rustls::ALL_VERSIONS);
    let config = c.client.clone();
    with_server(c.acceptor, impatient, move |addr| {
        let mut client = connect(addr, &config);
        let mut expected = Vec::new();
        for i in 0..60u32 {
            let piece = format!("piece-{i};").into_bytes();
            client.write_all(&piece).unwrap();
            client.flush().unwrap();
            expected.extend_from_slice(&piece);
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut got = vec![0u8; expected.len()];
        client.read_exact(&mut got).unwrap();
        assert_eq!(got, expected);
    });
}

#[test]
fn bad_handshakes_fail_without_harming_the_server() {
    let c = certs(rustls::ALL_VERSIONS);
    let acceptor = c.acceptor.handshake_timeout(Duration::from_millis(300));
    let config = c.client.clone();
    with_server(acceptor, echo, move |addr| {
        // Not TLS at all.
        let mut plain = TcpStream::connect(addr).unwrap();
        plain
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        plain
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut reply = Vec::new();
        let _ = plain.read_to_end(&mut reply);
        assert!(
            !reply.starts_with(b"HTTP/"),
            "answered plain HTTP on a TLS port"
        );

        // A client that connects and never speaks is dropped after the
        // handshake timeout.
        let mut silent = TcpStream::connect(addr).unwrap();
        silent
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let start = std::time::Instant::now();
        let mut buf = [0u8; 16];
        assert_eq!(silent.read(&mut buf).unwrap_or(0), 0);
        assert!(start.elapsed() >= Duration::from_millis(250));
        assert!(start.elapsed() < Duration::from_secs(5));

        // A client that does not trust the certificate.
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let distrustful = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(RootCertStore::empty())
            .with_no_client_auth();
        let mut client = connect(addr, &Arc::new(distrustful));
        assert!(client.write_all(b"x").is_err());

        // The server still works. (On a slow machine a handshake may take
        // longer than the short timeout, so try a few times.)
        let alive = (0..20).any(|_| {
            let mut client = connect(addr, &config);
            let mut buf = [0u8; 11];
            client.write_all(b"still alive").is_ok()
                && client.read_exact(&mut buf).is_ok()
                && &buf == b"still alive"
        });
        assert!(alive);
    });
}

#[test]
fn one_task_may_receive_while_another_sends() {
    // The echo task and a second task write to the same connection at the
    // same time; the client would fail to decrypt interleaved records.
    const TICKS: usize = 200;
    async fn split(tls: TlsStream) {
        let tls = std::rc::Rc::new(tls);
        let ticker = {
            let tls = tls.clone();
            giong::spawn(async move {
                for _ in 0..TICKS {
                    if tls.write_all(vec![b'T'; 20_000]).await.0.is_err() {
                        return;
                    }
                    giong::yield_now().await;
                }
            })
        };
        while let Ok(data) = tls.recv().await {
            if data.is_empty() || tls.write_all(data).await.0.is_err() {
                break;
            }
        }
        let _ = ticker.await;
        let _ = tls.shutdown().await;
    }
    let c = certs(rustls::ALL_VERSIONS);
    let config = c.client.clone();
    with_server(c.acceptor, split, move |addr| {
        let mut client = connect(addr, &config);
        let writer_tcp = client.sock.try_clone().unwrap();
        client.write_all(b"e").unwrap(); // completes the handshake
        let echoed = 300 * 10_000 + 1;
        let total = echoed + TICKS * 20_000;
        let (mut ticks, mut echoes) = (0usize, 0usize);
        let mut chunk = vec![0u8; 64 * 1024];
        let mut sent = 0;
        // Send in pieces while reading, so that neither side stalls.
        while ticks + echoes < total {
            if sent < 300 {
                client.write_all(&[b'e'; 10_000]).unwrap();
                client.flush().unwrap();
                sent += 1;
            }
            let n = client.read(&mut chunk).unwrap();
            assert!(n > 0, "closed after {} of {total} bytes", ticks + echoes);
            ticks += chunk[..n].iter().filter(|&&b| b == b'T').count();
            echoes += chunk[..n].iter().filter(|&&b| b == b'e').count();
        }
        assert_eq!((ticks, echoes), (TICKS * 20_000, echoed));
        drop(writer_tcp);
    });
}

#[test]
fn a_cancelled_write_poisons_the_connection_instead_of_corrupting_it() {
    let (tx, rx) = mpsc::channel();
    let c = certs(rustls::ALL_VERSIONS);
    let config = c.client.clone();
    let serve = move |tls: TlsStream| {
        let tx = tx.clone();
        async move {
            // Wait for the client's first byte, then write far more than the
            // socket buffers hold to a client that is not reading.
            let _ = tls.recv().await;
            let big = vec![7u8; 64 << 20];
            let cancelled = timeout(Duration::from_millis(100), tls.write_all(big))
                .await
                .is_err();
            let later = tls.write_all(b"more".to_vec()).await.0;
            let _ = tx.send((cancelled, later.is_err()));
            // What the client sent before can still be received.
            let _ = tx.send((tls.recv().await.ok() == Some(b"second".to_vec()), true));
        }
    };
    with_server(c.acceptor, serve, move |addr| {
        let mut client = connect(addr, &config);
        client.write_all(b"x").unwrap();
        client.flush().unwrap();
        let wait = Duration::from_secs(10);
        assert_eq!(rx.recv_timeout(wait).unwrap(), (true, true));
        client.write_all(b"second").unwrap();
        client.flush().unwrap();
        assert_eq!(rx.recv_timeout(wait).unwrap(), (true, true));

        // The server has dropped the connection in the middle of a record.
        // The client must see a cut-off stream, never the bytes of "more"
        // spliced into the record that was being sent.
        let mut sink = vec![0u8; 1 << 20];
        let err = loop {
            match client.read(&mut sink) {
                Ok(0) => panic!("a cut-off stream looked like a clean close"),
                Ok(n) => assert!(sink[..n].iter().all(|&b| b == 7)),
                Err(e) => break e,
            }
        };
        assert!(
            matches!(
                err.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
            ),
            "{err}"
        );
    });
}

#[test]
fn shutdown_after_a_cancelled_write_still_closes_the_connection() {
    let (tx, rx) = mpsc::channel();
    let c = certs(rustls::ALL_VERSIONS);
    let config = c.client.clone();
    let serve = move |tls: TlsStream| {
        let tx = tx.clone();
        async move {
            let _ = tls.recv().await;
            let big = vec![7u8; 64 << 20];
            let cancelled = timeout(Duration::from_millis(100), tls.write_all(big))
                .await
                .is_err();
            // No close_notify can follow a cut record, but the TCP side
            // closes, while the stream itself stays open here.
            let shutdown = tls.shutdown().await;
            let _ = tx.send((cancelled, shutdown.is_ok()));
            let _ = tls.recv().await;
        }
    };
    with_server(c.acceptor, serve, move |addr| {
        let mut client = connect(addr, &config);
        client.write_all(b"x").unwrap();
        client.flush().unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            (true, true)
        );
        let mut sink = vec![0u8; 1 << 20];
        let err = loop {
            match client.read(&mut sink) {
                Ok(0) => panic!("a cut-off stream looked like a clean close"),
                Ok(_) => {}
                Err(e) => break e,
            }
        };
        // Not a read timeout: the server's FIN arrived.
        assert_eq!(err.kind(), std::io::ErrorKind::UnexpectedEof, "{err}");
    });
}

#[test]
fn close_notify_is_told_apart_from_a_cut_connection() {
    // The server echoes what it receives and, at the end of the stream,
    // reports everything it got and whether the end was a close_notify.
    let (tx, rx) = mpsc::channel();
    let serve = move |tls: TlsStream| {
        let tx = tx.clone();
        async move {
            let mut got = Vec::new();
            while let Ok(data) = tls.recv().await {
                if data.is_empty() || tls.write_all(data.clone()).await.0.is_err() {
                    break;
                }
                got.extend_from_slice(&data);
            }
            let _ = tx.send((got, tls.received_close_notify()));
        }
    };
    let c = certs(rustls::ALL_VERSIONS);
    let config = c.client.clone();
    with_server(c.acceptor, serve, move |addr| {
        let wait = Duration::from_secs(10);
        // Reading the echo first also reads whatever else the server sent
        // (TLS 1.3 session tickets), so closing the socket sends a FIN, not a
        // reset.
        let exchange = |client: &mut Client, data: &[u8]| {
            client.write_all(data).unwrap();
            client.flush().unwrap();
            let mut back = vec![0u8; data.len()];
            client.read_exact(&mut back).unwrap();
            assert_eq!(back, data);
        };

        // A clean end: close_notify, then the socket.
        let mut client = connect(addr, &config);
        exchange(&mut client, b"all of it");
        client.conn.send_close_notify();
        while client.conn.wants_write() {
            client.conn.write_tls(&mut client.sock).unwrap();
        }
        drop(client);
        assert_eq!(
            rx.recv_timeout(wait).unwrap(),
            (b"all of it".to_vec(), true)
        );

        // Cut short: the socket closes without a close_notify, as when
        // someone on the path injects a FIN.
        let mut client = connect(addr, &config);
        exchange(&mut client, b"the first half");
        client.sock.shutdown(std::net::Shutdown::Both).unwrap();
        assert_eq!(
            rx.recv_timeout(wait).unwrap(),
            (b"the first half".to_vec(), false)
        );
    });
}

/// A socket that writes one byte at a time, so that every TLS record reaches
/// the server in many pieces.
struct Trickle(TcpStream);

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Trickle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match buf.first() {
            Some(b) => self.0.write(std::slice::from_ref(b)),
            None => Ok(0),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[test]
fn records_split_into_single_bytes_are_put_back_together() {
    for versions in [
        &[&rustls::version::TLS12][..],
        &[&rustls::version::TLS13][..],
    ] {
        let c = certs(versions);
        let config = c.client.clone();
        with_server(c.acceptor, echo, move |addr| {
            let tcp = TcpStream::connect(addr).unwrap();
            tcp.set_nodelay(true).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let name = ServerName::try_from("localhost").unwrap();
            let conn = ClientConnection::new(config.clone(), name).unwrap();
            let mut client = StreamOwned::new(conn, Trickle(tcp));
            let message: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
            client.write_all(&message).unwrap();
            client.flush().unwrap();
            let mut back = vec![0u8; message.len()];
            client.read_exact(&mut back).unwrap();
            assert!(back == message);
        });
    }
}

#[test]
fn garbage_after_the_handshake_is_an_error_not_data() {
    let (tx, rx) = mpsc::channel();
    let serve = move |tls: TlsStream| {
        let tx = tx.clone();
        async move {
            let mut got = Vec::new();
            let end = loop {
                match tls.recv().await {
                    Ok(data) if data.is_empty() => break Ok(()),
                    Ok(data) => got.extend_from_slice(&data),
                    Err(e) => break Err(e.kind()),
                }
                let _ = tls.write_all(b"ok".to_vec()).await;
            };
            let _ = tx.send((got, end));
        }
    };
    let c = certs(rustls::ALL_VERSIONS);
    let config = c.client.clone();
    with_server(c.acceptor, serve, move |addr| {
        let mut client = connect(addr, &config);
        client.write_all(b"real").unwrap();
        client.flush().unwrap();
        let mut ok = [0u8; 2];
        client.read_exact(&mut ok).unwrap();
        // An application-data record header followed by bytes that are not a
        // valid encryption of anything.
        let mut forged = vec![0x17, 0x03, 0x03, 0x00, 0x40];
        forged.extend((0..0x40u8).map(|i| i.wrapping_mul(37)));
        client.sock.write_all(&forged).unwrap();
        let (got, end) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(got, b"real", "forged bytes were handed out as data");
        assert_eq!(end, Err(std::io::ErrorKind::InvalidData));
        // The server told the client why, then closed: the client is not left
        // waiting (its read timeout would show as WouldBlock or TimedOut).
        let mut rest = Vec::new();
        if let Err(e) = client.sock.read_to_end(&mut rest) {
            use std::io::ErrorKind::{TimedOut, WouldBlock};
            assert!(!matches!(e.kind(), WouldBlock | TimedOut), "{e}");
        }
    });
}

#[test]
fn random_bytes_instead_of_a_handshake_never_hang_or_harm_the_server() {
    let c = certs(rustls::ALL_VERSIONS);
    let acceptor = c.acceptor.handshake_timeout(Duration::from_secs(2));
    let config = c.client.clone();
    with_server(acceptor, echo, move |addr| {
        let mut state = 0x2545_f491u32;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for round in 0..40 {
            let len = 1 + next() as usize % 3000;
            let mut bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            // Half of them start like a TLS handshake record, to get past the
            // first check.
            if round % 2 == 0 && len > 5 {
                bytes[..3].copy_from_slice(&[0x16, 0x03, 0x01]);
            }
            let mut tcp = TcpStream::connect(addr).unwrap();
            tcp.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let _ = tcp.write_all(&bytes);
            let start = std::time::Instant::now();
            let mut sink = Vec::new();
            let _ = tcp.read_to_end(&mut sink);
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "round {round}: the server kept the connection open"
            );
            assert!(
                sink.len() < 64,
                "round {round}: answered with {} bytes",
                sink.len()
            );
        }
        let mut client = connect(addr, &config);
        client.write_all(b"still alive").unwrap();
        let mut buf = [0u8; 11];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"still alive");
    });
}
