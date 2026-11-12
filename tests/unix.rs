//! Unix domain stream sockets.

use std::io::{ErrorKind, Read, Write};
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

#[test]
fn bind_and_connect_errors() {
    let path = SocketPath::new("errors");
    giong::block_on(async {
        // Nobody is listening.
        let err = UnixStream::connect(&path.0).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        // The file is in the way, as with the standard library.
        let listener = UnixListener::bind(&path.0).unwrap();
        let err = UnixListener::bind(&path.0).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AddrInUse);

        // A socket file whose listener is gone refuses connections.
        drop(listener);
        giong::yield_now().await;
        let err = UnixStream::connect(&path.0).await.unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionRefused);
    });
}

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
        stream.write_all(&data).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        let mut back = Vec::new();
        stream.read_to_end(&mut back).unwrap();
        assert!(back == data);
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

#[test]
fn many_connections_at_once() {
    let path = SocketPath::new("many");
    giong::block_on(async {
        const CONNS: usize = 300;
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
