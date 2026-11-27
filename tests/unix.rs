//! Unix domain stream sockets.

use std::io::ErrorKind;
#[cfg(unix)]
use std::io::{Read, Write};
use std::net::Shutdown;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use giong::net::{UnixListener, UnixStream};
use giong::time::timeout;

/// A socket path of its own for one test, removed afterwards.
struct SocketPath(PathBuf);

impl SocketPath {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("giong-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Deterministic pseudo-random payload.
fn payload(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn recv_exact(stream: &UnixStream, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let chunk = stream.recv().await.unwrap();
        assert!(
            !chunk.is_empty(),
            "unexpected EOF after {} bytes",
            out.len()
        );
        out.extend_from_slice(&chunk);
    }
    out
}

async fn echo(stream: UnixStream) {
    loop {
        let data = stream.recv().await.unwrap();
        if data.is_empty() {
            return;
        }
        let copy = data.to_vec();
        drop(data);
        stream.write_all(copy).await.0.unwrap();
    }
}

#[test]
fn echo_over_a_socket_file() {
    let path = SocketPath::new("echo");
    giong::block_on(async {
        let listener = UnixListener::bind(&path.0).unwrap();
        let local = listener.local_addr().unwrap();
        assert_eq!(local.as_pathname(), Some(path.0.as_path()));

        let server = giong::spawn(async move {
            let stream = listener.accept().await.unwrap();
            // The client did not bind: its address has no name.
            assert!(stream.peer_addr().unwrap().is_unnamed());
            echo(stream).await;
        });

        let client = UnixStream::connect(&path.0).await.unwrap();
        let peer = client.peer_addr().unwrap();
        assert_eq!(peer.as_pathname(), Some(path.0.as_path()));
        for round in 0..50u64 {
            let data = payload(1 + (round as usize * 37) % 5000, round);
            client.write_all(data.clone()).await.0.unwrap();
            assert!(recv_exact(&client, data.len()).await == data);
        }
        client.shutdown(Shutdown::Write).unwrap();
        assert!(client.recv().await.unwrap().is_empty());
        server.await.unwrap();
    });
}

#[test]
fn megabytes_in_both_directions_over_a_pair() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let (a, b) = (Rc::new(a), Rc::new(b));
        const LEN: usize = 4 * 1024 * 1024;
        let send = |stream: Rc<UnixStream>, seed| {
            giong::spawn(async move { stream.write_all(payload(LEN, seed)).await.0.unwrap() })
        };
        let (sent_a, sent_b) = (send(a.clone(), 1), send(b.clone(), 2));
        let recv =
            |stream: Rc<UnixStream>| giong::spawn(async move { recv_exact(&stream, LEN).await });
        let (got_a, got_b) = (recv(a.clone()), recv(b.clone()));
        assert!(got_a.await.unwrap() == payload(LEN, 2));
        assert!(got_b.await.unwrap() == payload(LEN, 1));
        sent_a.await.unwrap();
        sent_b.await.unwrap();
    });
}

#[test]
fn vectored_writes_over_a_pair() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let pieces: Vec<Vec<u8>> = (0..2000).map(|i| payload(i * 3, i as u64)).collect();
        let expected = pieces.concat();
        let b = Rc::new(b);
        let reader = {
            let (b, len) = (b.clone(), expected.len());
            giong::spawn(async move { recv_exact(&b, len).await })
        };
        let (res, back) = a.write_all_vectored(pieces).await;
        res.unwrap();
        assert_eq!(back.len(), 2000);
        assert!(reader.await.unwrap() == expected);
        let (res, _) = a.write_vectored(vec![&b"x"[..], &b"y"[..]]).await;
        assert_eq!(res.unwrap(), 2);
        assert_eq!(recv_exact(&b, 2).await, b"xy");
    });
}

#[test]
fn recv_is_cancel_safe() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        for _ in 0..5 {
            assert!(timeout(Duration::from_millis(2), b.recv()).await.is_err());
        }
        a.write_all("first").await.0.unwrap();
        giong::time::sleep(Duration::from_millis(20)).await;
        let data = timeout(Duration::from_secs(5), b.recv()).await;
        assert_eq!(&*data.unwrap().unwrap(), b"first");
    });
}

#[test]
fn one_off_reads_work_and_do_not_mix_with_recv() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        a.write_all("hello").await.0.unwrap();
        let (res, buf) = b.read(Vec::with_capacity(64)).await;
        assert_eq!((res.unwrap(), &buf[..]), (5, &b"hello"[..]));
        let (res, _) = a.write(&b"x"[..]).await;
        assert_eq!(res.unwrap(), 1);
        assert_eq!(&*b.recv().await.unwrap(), b"x");
        let (res, _) = b.read(buf).await;
        assert_eq!(res.unwrap_err().kind(), ErrorKind::InvalidInput);
    });
}

/// `recv_into` mixes with `recv` on one stream: a body whose first bytes
/// came through `recv` is completed straight into its own buffer, every byte
/// in order, and `recv` works again after it.
#[test]
fn recv_into_completes_a_body_after_recv() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        const LEN: usize = 1 << 20;
        let expected = payload(LEN, 7);
        let writer = giong::spawn({
            let data = expected.clone();
            async move {
                a.write_all(data).await.0.unwrap();
                a
            }
        });
        let head = b.recv().await.unwrap();
        let mut body = Vec::with_capacity(LEN);
        body.extend_from_slice(&head);
        drop(head);
        while body.len() < LEN {
            let (res, buf) = b.recv_into(body).await;
            body = buf;
            assert!(res.unwrap() > 0, "the stream ended at {}", body.len());
        }
        assert!(body == expected);
        let a = writer.await.unwrap();
        a.write_all("tail").await.0.unwrap();
        assert_eq!(&*b.recv().await.unwrap(), b"tail");
    });
}

#[test]
fn bind_and_connect_errors() {
    let path = SocketPath::new("errors");
    giong::block_on(async {
        // Nobody is listening. Windows refuses the connection rather than
        // saying the file is missing; the system's error is passed on.
        let err = UnixStream::connect(&path.0).await.unwrap_err();
        #[cfg(unix)]
        assert_eq!(err.kind(), ErrorKind::NotFound);
        #[cfg(windows)]
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused);

        // The file is in the way, as with the standard library.
        let listener = UnixListener::bind(&path.0).unwrap();
        let err = UnixListener::bind(&path.0).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
        // Windows says why, and how to get out of it.
        #[cfg(windows)]
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::UnixSocketInUse),
            "{err}"
        );

        // A socket file whose listener is gone refuses connections.
        drop(listener);
        giong::yield_now().await;
        let err = UnixStream::connect(&path.0).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused);
    });
}

// The standard library has Unix sockets on Unix only.
#[cfg(unix)]
#[test]
fn works_with_the_standard_library_on_the_other_side() {
    let path = SocketPath::new("std");
    let client_path = path.0.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let client = std::thread::spawn(move || {
        ready_rx.recv().unwrap();
        let mut stream = std::os::unix::net::UnixStream::connect(client_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let data = payload(300_000, 9);
        // Read while writing: the echo stops reading when what it sent back
        // is not taken, and how much the two sockets hold in between differs
        // by system (a few kilobytes on macOS).
        let mut reading = stream.try_clone().unwrap();
        let reader = std::thread::spawn(move || {
            let mut back = Vec::new();
            reading.read_to_end(&mut back).unwrap();
            back
        });
        stream.write_all(&data).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        assert!(reader.join().unwrap() == data);
    });
    giong::block_on(async {
        // A listener made by the standard library, handed to giong.
        let std_listener = std::os::unix::net::UnixListener::bind(&path.0).unwrap();
        let listener = UnixListener::from_std(std_listener);
        ready_tx.send(()).unwrap();
        echo(listener.accept().await.unwrap()).await;
    });
    client.join().unwrap();
}

/// Linux 6.18 (seen on 6.18.44 and 6.18.63, not on 6.12.117) leaves a
/// multishot receive on a Unix socket waiting when the last data arrived
/// together with the peer's `shutdown(Write)` and more than 32 buffers of it
/// were unread: the end of the stream was never reported, and this reader
/// hung until the peer closed.
#[cfg(unix)]
#[test]
fn the_end_of_the_stream_is_seen_after_data_that_was_waiting() {
    const BUFFER: usize = 4096;
    giong::Builder::new()
        .recv_buffers(512, BUFFER)
        .block_on(async {
            // A first transfer, so that the worker keeps enough buffers with
            // the kernel for the second one to be received in one go.
            let (mut peer, ours) = std::os::unix::net::UnixStream::pair().unwrap();
            let warm_up = payload(4 << 20, 12);
            let sent = warm_up.clone();
            let writer = std::thread::spawn(move || {
                peer.write_all(&sent).unwrap();
            });
            let stream = UnixStream::from_std(ours);
            assert!(recv_exact(&stream, warm_up.len()).await == warm_up);
            assert!(stream.recv().await.unwrap().is_empty());
            writer.join().unwrap();

            // More buffers of data than the kernel receives in one pass,
            // little enough to wait in a socket nobody reads yet.
            let data = payload(40 * BUFFER, 11);
            let (mut peer, ours) = std::os::unix::net::UnixStream::pair().unwrap();
            let sent = data.clone();
            let writer = std::thread::spawn(move || {
                peer.write_all(&sent).unwrap();
                peer.shutdown(Shutdown::Write).unwrap();
                peer
            });
            // Linux holds all of it in the socket, so it is all there with
            // the shutdown before the first receive, as the kernel's fault
            // needs. A macOS socket holds a few kilobytes: there the writer
            // goes on while this reads.
            #[cfg(target_os = "linux")]
            let peer = writer.join().unwrap();
            let stream = UnixStream::from_std(ours);
            let mut received = Vec::new();
            loop {
                let piece = timeout(Duration::from_secs(10), stream.recv())
                    .await
                    .expect("the end of the stream was not reported")
                    .unwrap();
                if piece.is_empty() {
                    break;
                }
                received.extend_from_slice(&piece);
            }
            assert!(received == data);
            #[cfg(not(target_os = "linux"))]
            let peer = writer.join().unwrap();
            // Open until here: closing it would have woken the receive.
            drop(peer);
        })
        .unwrap();
}

#[test]
fn many_connections_at_once() {
    let path = SocketPath::new("many");
    giong::block_on(async {
        // macOS refuses a connection once 128 wait on the listener
        // (`kern.ipc.somaxconn`); Linux makes the connect wait instead.
        const CONNS: usize = if cfg!(target_os = "macos") { 100 } else { 300 };
        let listener = UnixListener::bind(&path.0).unwrap();
        giong::spawn(async move {
            loop {
                let stream = listener.accept().await.unwrap();
                giong::spawn(echo(stream));
            }
        });
        let clients: Vec<_> = (0..CONNS)
            .map(|i| {
                let path = path.0.clone();
                giong::spawn(async move {
                    let stream = UnixStream::connect(path).await.unwrap();
                    let data = payload(2000 + i, i as u64);
                    stream.write_all(data.clone()).await.0.unwrap();
                    assert!(recv_exact(&stream, data.len()).await == data);
                })
            })
            .collect();
        for client in clients {
            client.await.unwrap();
        }
    });
}

#[test]
fn addresses_name_the_listener_and_leave_the_client_unnamed() {
    let path = SocketPath::new("addrs");
    giong::block_on(async {
        let listener = UnixListener::bind(&path.0).unwrap();
        let addr = listener.local_addr().unwrap();
        assert_eq!(addr.as_pathname(), Some(path.0.as_path()));
        let client = UnixStream::connect(&path.0).await.unwrap();
        let server = listener.accept().await.unwrap();
        assert_eq!(
            client.peer_addr().unwrap().as_pathname(),
            Some(path.0.as_path())
        );
        assert!(client.local_addr().unwrap().is_unnamed());
        assert!(server.peer_addr().unwrap().is_unnamed());
    });
}

#[test]
fn a_pair_is_connected_both_ways_and_leaves_no_file() {
    giong::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        a.write_all(b"ping".to_vec()).await.0.unwrap();
        assert_eq!(recv_exact(&b, 4).await, b"ping");
        b.write_all(b"pong".to_vec()).await.0.unwrap();
        assert_eq!(recv_exact(&a, 4).await, b"pong");
        a.shutdown(Shutdown::Write).unwrap();
        assert!(b.recv().await.unwrap().is_empty());
        // Windows makes the pair through a listener whose file is gone.
        #[cfg(windows)]
        if let Some(path) = b.local_addr().unwrap().as_pathname() {
            assert!(!path.exists(), "{path:?} was left behind");
        }
    });
}

#[cfg(windows)]
#[test]
fn windows_refuses_paths_it_cannot_hold_with_a_code() {
    let long = std::env::temp_dir().join("x".repeat(120));
    giong::block_on(async {
        let err = UnixListener::bind(&long).unwrap_err();
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::UnixPathTooLong),
            "{err}"
        );
        let err = UnixStream::connect(&long).await.unwrap_err();
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::UnixPathTooLong),
            "{err}"
        );
    });
}

/// Connects and accepts dropped mid-flight give back everything they held,
/// on every driver (on Windows the connect is a `ConnectEx` and the accept
/// an `AcceptEx`, both cancelled through the completion port).
#[test]
fn dropped_connects_and_accepts_leave_nothing_behind() {
    let path = SocketPath::new("dropped");
    giong::block_on(async {
        let listener = UnixListener::bind(&path.0).unwrap();
        // An accept nobody connects to.
        assert!(timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err());
        // Connects the listener never accepts, dropped wherever they are.
        for _ in 0..50 {
            let _ = timeout(Duration::from_micros(1), UnixStream::connect(&path.0)).await;
        }
        drop(listener);
        giong::time::sleep(Duration::from_millis(50)).await;
        let stats = giong::observe::worker_stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

/// As over TCP: shutting down the read half ends a `recv` that waits.
#[test]
fn shutting_down_the_read_half_ends_a_waiting_recv() {
    giong::block_on(async {
        let (a, _b) = UnixStream::pair().unwrap();
        let a = Rc::new(a);
        let reader = {
            let a = a.clone();
            giong::spawn(async move { a.recv().await })
        };
        giong::time::sleep(Duration::from_millis(50)).await;
        a.shutdown(Shutdown::Read).unwrap();
        let got = timeout(Duration::from_secs(5), reader)
            .await
            .expect("the recv ended")
            .unwrap()
            .unwrap();
        assert!(got.is_empty(), "the end of the stream");
    });
}

#[test]
fn a_read_or_recv_after_the_read_half_is_shut_gives_the_end_of_the_stream() {
    giong::block_on(async {
        // `read` on one stream, `recv` on another: a stream takes one kind.
        let (reading, _a) = UnixStream::pair().unwrap();
        let (receiving, _b) = UnixStream::pair().unwrap();
        reading.shutdown(Shutdown::Read).unwrap();
        receiving.shutdown(Shutdown::Read).unwrap();
        for _ in 0..2 {
            let (res, _) = timeout(Duration::from_secs(5), reading.read(Vec::with_capacity(64)))
                .await
                .expect("the read ended");
            assert_eq!(res.unwrap(), 0, "the end of the stream");
            let got = timeout(Duration::from_secs(5), receiving.recv())
                .await
                .expect("the recv ended")
                .unwrap();
            assert!(got.is_empty(), "the end of the stream");
        }
    });
}
