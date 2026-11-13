//! TCP: accept, recv (multishot + provided buffers), read, write, close.

use std::borrow::Cow;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr};
use std::rc::Rc;
use std::time::Duration;

use giong::net::{TcpListener, TcpStream};
use giong::time::{sleep, timeout};

mod common;

/// Lets this process open at least `needed` descriptors, by raising the soft
/// limit up to the hard one. Shells start with a soft limit of 1024 (Ubuntu's
/// included), which the tests below need more than; the tests running
/// alongside open some, too.
fn allow_open_files(needed: u64) {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: plain system calls with a valid `rlimit`.
    unsafe {
        assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim), 0);
        if lim.rlim_cur >= needed {
            return;
        }
        assert!(
            lim.rlim_max >= needed,
            "this test needs {needed} open files, and the hard limit is {} (ulimit -Hn)",
            lim.rlim_max
        );
        // As much as allowed, so that the other tests have room too; the
        // kernel caps it at fs.nr_open, so fall back to what is needed.
        let hard = lim.rlim_max;
        lim.rlim_cur = hard;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &lim) != 0 {
            lim.rlim_cur = needed;
            assert_eq!(
                libc::setrlimit(libc::RLIMIT_NOFILE, &lim),
                0,
                "{}",
                std::io::Error::last_os_error()
            );
        }
    }
}

fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
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

async fn recv_exact(stream: &TcpStream, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let chunk = stream.recv().await.unwrap();
        assert!(
            !chunk.is_empty(),
            "unexpected EOF after {} of {len} bytes",
            out.len()
        );
        out.extend_from_slice(&chunk);
    }
    out
}

#[test]
fn echo_roundtrip() {
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let s = l.accept().await.unwrap();
            loop {
                let data = s.recv().await.unwrap();
                if data.is_empty() {
                    break;
                }
                let copy = data.to_vec();
                drop(data);
                s.write_all(copy).await.0.unwrap();
            }
        });

        let c = TcpStream::connect(addr).await.unwrap();
        for i in 0..50 {
            let msg = format!("hello {i}").into_bytes();
            let (res, msg) = c.write_all(msg).await;
            res.unwrap();
            assert_eq!(recv_exact(&c, msg.len()).await, msg);
        }
        drop(c);
        server.await.unwrap();

        let stats = giong::stats();
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

#[test]
fn large_transfer_is_intact_in_both_directions() {
    const LEN: usize = 8 * 1024 * 1024;
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let s = l.accept().await.unwrap();
            let got = recv_exact(&s, LEN).await;
            assert_eq!(got, payload(LEN, 1));
            s.write_all(payload(LEN, 2)).await.0.unwrap();
        });

        let c = Rc::new(TcpStream::connect(addr).await.unwrap());
        // Write and read concurrently so neither side stalls on full buffers.
        let writer = {
            let c = c.clone();
            giong::spawn(async move { c.write_all(payload(LEN, 1)).await.0.unwrap() })
        };
        let got = recv_exact(&c, LEN).await;
        assert_eq!(got, payload(LEN, 2));
        writer.await.unwrap();
        server.await.unwrap();

        let stats = giong::stats();
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

#[test]
fn a_reader_that_keeps_up_gets_a_bulk_transfer_without_copies() {
    // One stream, written 256 KiB at a time: dozens of receives complete
    // between two turns of the event loop. None of it may be moved out of
    // the pool while the pool has room (a backlog limit of 8 per stream
    // once copied most of such a transfer).
    const LEN: usize = 64 * 1024 * 1024;
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    let writer = std::thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        let chunk = vec![5u8; 256 * 1024];
        for _ in 0..LEN / chunk.len() {
            s.write_all(&chunk).unwrap();
        }
    });
    giong::block_on(async move {
        let l = TcpListener::from_std(std_listener);
        let s = l.accept().await.unwrap();
        let mut got = 0;
        while got < LEN {
            let data = s.recv().await.unwrap();
            assert!(!data.is_empty(), "EOF after {got} bytes");
            assert!(data.iter().all(|&b| b == 5));
            got += data.len();
        }
        let stats = giong::stats();
        assert_eq!(stats.recv_copied, 0, "{stats:?}");
    });
    writer.join().unwrap();
}

#[test]
fn large_writes_arrive_intact_with_or_without_zero_copy() {
    // Sizes around the zero-copy threshold (16 KiB), written plainly and as
    // a header and a body, twice on each fresh connection: the first large
    // send of a socket goes out without a copy where the kernel offers it;
    // on loopback the kernel copies it after all, so the second is a plain
    // send.
    giong::block_on(async {
        let (l, addr) = listener();
        let sizes = [16 * 1024 - 1, 16 * 1024, 64 * 1024 + 7, 1024 * 1024 + 3];
        for (i, &size) in sizes.iter().enumerate() {
            for vectored in [false, true] {
                let c = TcpStream::connect(addr).await.unwrap();
                let s = l.accept().await.unwrap();
                let data = payload(size, i as u64);
                let reader = giong::spawn(async move { recv_exact(&s, 2 * size).await });
                for _ in 0..2 {
                    if vectored {
                        let (head, body) = data.split_at(100);
                        let pieces = vec![head.to_vec(), body.to_vec()];
                        c.write_all_vectored(pieces).await.0.unwrap();
                    } else {
                        c.write_all(data.clone()).await.0.unwrap();
                    }
                }
                let got = reader.await.unwrap();
                assert!(
                    got[..size] == data[..] && got[size..] == data[..],
                    "{size} bytes, vectored: {vectored}"
                );
            }
        }
        drop(l);
        sleep(Duration::from_millis(20)).await;
        let stats = giong::stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}

#[test]
fn recv_reports_eof_and_keeps_reporting_it() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(&b"bye"[..]).await.0.unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        assert_eq!(&*s.recv().await.unwrap(), b"bye");
        assert!(s.recv().await.unwrap().is_empty());
        assert!(s.recv().await.unwrap().is_empty());
    });
}

#[test]
fn read_with_an_owned_buffer() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(String::from("owned buffer")).await.0.unwrap();
        let (res, buf) = s.read(Vec::with_capacity(64)).await;
        assert_eq!(res.unwrap(), 12);
        assert_eq!(buf, b"owned buffer");
        drop(c);
        let (res, buf) = s.read(buf).await;
        assert_eq!(res.unwrap(), 0);
        assert!(buf.is_empty());
    });
}

#[test]
fn read_and_recv_cannot_be_mixed() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(&b"x"[..]).await.0.unwrap();
        assert_eq!(&*s.recv().await.unwrap(), b"x");
        let (res, _) = s.read(Vec::with_capacity(8)).await;
        assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
    });
}

#[test]
fn connect_refused() {
    giong::block_on(async {
        let (_refusing, addr) = common::refusing_addr();
        let err = TcpStream::connect(addr).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);
        assert_eq!(giong::stats().tracked_ops, 0);
    });
}

#[test]
fn vectored_writes_send_every_piece_in_order() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = Rc::new(l.accept().await.unwrap());

        // 3,001 pieces (one sendmsg takes 1,024), static, owned and empty,
        // and 6 MiB in one of them, so the socket buffer fills and sends end
        // part-way through a piece.
        let mut pieces: Vec<Cow<'static, [u8]>> = Vec::new();
        for i in 0..3000u64 {
            pieces.push(match i % 3 {
                0 => Cow::Borrowed(b"static;"),
                1 => Cow::Owned(payload((i % 700) as usize, i)),
                _ => Cow::Borrowed(b""),
            });
            if i == 1500 {
                pieces.push(Cow::Owned(payload(6 << 20, 99)));
            }
        }
        let expected = pieces.concat();
        let reader = {
            let s = s.clone();
            let len = expected.len();
            giong::spawn(async move { recv_exact(&s, len).await })
        };
        let (res, back) = c.write_all_vectored(pieces).await;
        res.unwrap();
        assert_eq!(back.len(), 3001);
        assert!(reader.await.unwrap() == expected);

        // Nothing to send is not an error and sends nothing.
        let (res, _) = c.write_vectored(Vec::<Vec<u8>>::new()).await;
        assert_eq!(res.unwrap(), 0);
        let (res, _) = c.write_all_vectored(vec![Vec::new(), Vec::new()]).await;
        res.unwrap();
        // A short one goes out whole.
        let (res, back) = c.write_vectored(vec![b"ab".to_vec(), b"cd".to_vec()]).await;
        assert_eq!(res.unwrap(), 4);
        assert_eq!(back, [b"ab".to_vec(), b"cd".to_vec()]);
        assert_eq!(recv_exact(&s, 4).await, b"abcd");
    });
}

#[test]
fn vectored_writes_to_a_closed_peer_are_an_error_not_a_signal() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        drop(l.accept().await.unwrap());
        sleep(Duration::from_millis(20)).await;
        let mut failed = false;
        for _ in 0..50 {
            let pieces = vec![vec![0u8; 32 * 1024], vec![1u8; 32 * 1024]];
            if c.write_all_vectored(pieces).await.0.is_err() {
                failed = true;
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert!(failed, "writes to a closed peer kept succeeding");
    });
}

#[test]
fn writing_to_a_closed_peer_is_an_error_not_a_signal() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        drop(s);
        sleep(Duration::from_millis(20)).await;
        let mut failed = false;
        for _ in 0..50 {
            if c.write_all(vec![0u8; 64 * 1024]).await.0.is_err() {
                failed = true;
                break;
            }
            sleep(Duration::from_millis(1)).await;
        }
        assert!(failed, "writes to a closed peer kept succeeding");
    });
}

#[test]
fn dropping_a_stream_closes_the_connection() {
    giong::block_on(async {
        let (l, addr) = listener();
        let c = TcpStream::connect(addr).await.unwrap();
        let s = l.accept().await.unwrap();
        // Arm the multishot receive, then drop the stream while it is in flight.
        assert!(timeout(Duration::from_millis(10), s.recv()).await.is_err());
        drop(s);
        let eof = timeout(Duration::from_secs(5), c.recv())
            .await
            .expect("peer never saw the close");
        assert!(eof.unwrap().is_empty());
        drop(c);
        drop(l); // its multishot accept is the only other tracked operation
        sleep(Duration::from_millis(20)).await;
        let stats = giong::stats();
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
        // On io_uring only the wake-up read may remain; epoll has nothing
        // in flight.
        let expected = match giong::driver_kind() {
            giong::DriverKind::IoUring => 1,
            _ => 0,
        };
        assert_eq!(stats.inflight, expected, "{stats:?}");
        assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
    });
}

#[test]
fn accept_many_connections() {
    const N: usize = 500;
    // Both ends of every connection, and room for the other tests.
    allow_open_files(2 * N as u64 + 1024);
    giong::block_on(async {
        let (l, addr) = listener();
        let server = giong::spawn(async move {
            let mut conns = Vec::new();
            for _ in 0..N {
                conns.push(l.accept().await.unwrap());
            }
            for (i, s) in conns.iter().enumerate() {
                s.write_all((i as u32).to_le_bytes().to_vec())
                    .await
                    .0
                    .unwrap();
            }
            conns.len()
        });
        let mut clients = Vec::new();
        for _ in 0..N {
            clients.push(TcpStream::connect(addr).await.unwrap());
        }
        assert_eq!(server.await.unwrap(), N);
        let mut seen = vec![false; N];
        for c in &clients {
            let id = u32::from_le_bytes(recv_exact(c, 4).await.try_into().unwrap()) as usize;
            assert!(!seen[id]);
            seen[id] = true;
        }
        assert!(seen.iter().all(|&s| s));
    });
}

#[test]
fn dropping_a_listener_with_pending_connections_closes_them() {
    giong::block_on(async {
        let (l, addr) = listener();
        // Arm the multishot accept.
        assert!(timeout(Duration::from_millis(10), l.accept())
            .await
            .is_err());
        let c1 = TcpStream::connect(addr).await.unwrap();
        let c2 = TcpStream::connect(addr).await.unwrap();
        sleep(Duration::from_millis(20)).await;
        // Two accepted sockets are queued inside the listener; dropping it must close them.
        drop(l);
        for c in [&c1, &c2] {
            let res = timeout(Duration::from_secs(5), c.recv())
                .await
                .expect("connection left open");
            assert!(matches!(res, Ok(ref b) if b.is_empty()) || res.is_err());
        }
        sleep(Duration::from_millis(20)).await;
        drop((c1, c2));
        sleep(Duration::from_millis(20)).await;
        assert_eq!(giong::stats().tracked_ops, 0);
    });
}

#[test]
fn interoperates_with_blocking_std_clients() {
    let (tx, rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        giong::block_on(async move {
            let (l, addr) = listener();
            tx.send(addr).unwrap();
            for _ in 0..3 {
                let s = l.accept().await.unwrap();
                giong::spawn(async move {
                    loop {
                        let data = s.recv().await.unwrap();
                        if data.is_empty() {
                            return;
                        }
                        let mut reply = data.to_vec();
                        drop(data);
                        reply.reverse();
                        s.write_all(reply).await.0.unwrap();
                    }
                });
            }
            sleep(Duration::from_millis(300)).await;
        });
    });
    let addr = rx.recv().unwrap();
    for _ in 0..3 {
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.write_all(b"abcdef").unwrap();
        let mut buf = [0u8; 6];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"fedcba");
    }
    server.join().unwrap();
}

#[test]
fn from_std_listener_and_stream() {
    giong::block_on(async {
        let std_l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = std_l.local_addr().unwrap();
        let l = TcpListener::from_std(std_l);
        let std_c = std::net::TcpStream::connect(addr).unwrap();
        let c = TcpStream::from_std(std_c);
        let s = l.accept().await.unwrap();
        assert_eq!(s.peer_addr().unwrap(), c.local_addr().unwrap());
        s.set_nodelay(true).unwrap();
        s.write_all(&b"ping"[..]).await.0.unwrap();
        assert_eq!(recv_exact(&c, 4).await, b"ping");
    });
}

#[test]
fn accept_backlog_is_bounded_and_no_connection_is_lost() {
    // More pending connections than the driver is willing to queue (1024):
    // the multishot accept must pause, leave the rest in the kernel's listen
    // backlog, and resume once the application catches up.
    const N: usize = 1300;
    // Every client, up to 1024 accepted sockets queued in the driver, and
    // room for the other tests.
    allow_open_files(N as u64 + 1024 + 1024);
    giong::block_on(async {
        let (l, addr) = listener();
        // Arm the multishot accept, then stop accepting for a while.
        assert!(timeout(Duration::from_millis(5), l.accept()).await.is_err());

        let mut clients = Vec::with_capacity(N);
        for i in 0..N {
            let c = TcpStream::connect(addr).await.unwrap();
            c.write_all((i as u32).to_le_bytes().to_vec())
                .await
                .0
                .unwrap();
            clients.push(c);
        }
        sleep(Duration::from_millis(50)).await;

        let mut seen = vec![false; N];
        for _ in 0..N {
            let s = timeout(Duration::from_secs(10), l.accept())
                .await
                .expect("accept stalled after the backlog pause")
                .unwrap();
            let id = u32::from_le_bytes(recv_exact(&s, 4).await.try_into().unwrap()) as usize;
            assert!(!seen[id], "connection {id} accepted twice");
            seen[id] = true;
        }
        assert!(seen.iter().all(|&s| s));
    });
}

/// Many connections writing small pieces at once through a small pool: with
/// the packed buffer ring (Linux 6.12+) the pieces of different connections
/// share buffers and are cut where a buffer ends. Every stream must still
/// arrive byte for byte, and every buffer must come back.
#[test]
fn small_writes_from_many_connections_share_buffers_and_arrive_intact() {
    const CONNS: usize = 16;
    const MESSAGES: usize = 300;
    fn stream_of(conn: usize) -> Vec<Vec<u8>> {
        (0..MESSAGES)
            .map(|i| {
                let len = 1 + (conn * 37 + i * 101) % 300;
                (0..len)
                    .map(|j| ((conn * 31 + i * 7 + j) % 251) as u8)
                    .collect()
            })
            .collect()
    }
    giong::Builder::new()
        .recv_buffers(8, 512)
        .block_on(async {
            let (l, addr) = listener();
            let mut writers = Vec::new();
            for conn in 0..CONNS {
                writers.push(giong::spawn(async move {
                    let c = TcpStream::connect(addr).await.unwrap();
                    c.write_all(vec![conn as u8]).await.0.unwrap();
                    for msg in stream_of(conn) {
                        c.write_all(msg).await.0.unwrap();
                        if conn % 3 == 0 {
                            giong::yield_now().await;
                        }
                    }
                }));
            }
            let mut readers = Vec::new();
            for _ in 0..CONNS {
                let s = l.accept().await.unwrap();
                readers.push(giong::spawn(async move {
                    let mut got = Vec::new();
                    loop {
                        let data = s.recv().await.unwrap();
                        if data.is_empty() {
                            break;
                        }
                        got.extend_from_slice(&data);
                        // Hold no piece across an await, but yield now and
                        // then so pieces of other streams pile up.
                        drop(data);
                        if got.len() % 5 == 0 {
                            giong::yield_now().await;
                        }
                    }
                    got
                }));
            }
            for w in writers {
                w.await.unwrap();
            }
            for r in readers {
                let got = timeout(Duration::from_secs(30), r).await.unwrap().unwrap();
                let conn = got[0] as usize;
                assert_eq!(&got[1..], stream_of(conn).concat(), "connection {conn}");
            }
            sleep(Duration::from_millis(20)).await;
            let stats = giong::stats();
            assert_eq!(stats.free_recv_buffers, stats.recv_buffers, "{stats:?}");
        })
        .unwrap();
}
