//! UDP: send_to, recv_from (multishot + provided buffers), read_from.

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use giong::net::UdpSocket;
use giong::time::{sleep, timeout};

fn socket() -> (UdpSocket, SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    (socket, addr)
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

#[test]
fn datagrams_roundtrip_with_their_sender() {
    giong::block_on(async {
        let (a, a_addr) = socket();
        let (b, b_addr) = socket();
        for round in 0..200u64 {
            let data = payload((round as usize * 53) % 1400, round);
            let (res, data) = a.send_to(data, b_addr).await;
            assert_eq!(res.unwrap(), data.len());
            let (got, from) = b.recv_from().await.unwrap();
            assert_eq!(from, a_addr);
            assert!(!got.is_truncated());
            assert!(*got == data[..]);
            drop(got);

            // And back, to the address the datagram came from.
            let (res, _) = b.send_to(&b"ack"[..], from).await;
            assert_eq!(res.unwrap(), 3);
            assert_eq!(&*a.recv().await.unwrap(), b"ack");
        }
    });
}

#[test]
fn an_empty_datagram_is_a_datagram() {
    giong::block_on(async {
        let (a, a_addr) = socket();
        let (b, b_addr) = socket();
        a.send_to(Vec::new(), b_addr).await.0.unwrap();
        a.send_to("after", b_addr).await.0.unwrap();
        let (first, from) = b.recv_from().await.unwrap();
        assert_eq!((first.len(), from), (0, a_addr));
        assert_eq!(&*b.recv().await.unwrap(), b"after");
    });
}

#[test]
fn ipv6_works_too() {
    giong::block_on(async {
        let Ok(a) = UdpSocket::bind("[::1]:0") else {
            eprintln!("no IPv6 loopback here; skipped");
            return;
        };
        let b = UdpSocket::bind("[::1]:0").unwrap();
        let (a_addr, b_addr) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        assert!(a_addr.is_ipv6());
        a.send_to("six", b_addr).await.0.unwrap();
        let (got, from) = b.recv_from().await.unwrap();
        assert_eq!((&*got, from), (&b"six"[..], a_addr));
    });
}

#[test]
fn connected_sockets_send_and_filter() {
    giong::block_on(async {
        let (a, a_addr) = socket();
        let (b, b_addr) = socket();
        let (stranger, _) = socket();
        a.connect(b_addr).unwrap();
        b.connect(a_addr).unwrap();
        assert_eq!(a.peer_addr().unwrap(), b_addr);

        // A connected socket only hears its peer.
        stranger.send_to("noise", b_addr).await.0.unwrap();
        a.send("signal").await.0.unwrap();
        assert_eq!(&*b.recv().await.unwrap(), b"signal");
        b.send(b"reply".to_vec()).await.0.unwrap();
        assert_eq!(&*a.recv().await.unwrap(), b"reply");
        assert!(timeout(Duration::from_millis(20), b.recv()).await.is_err());

        // Without a peer, `send` has nowhere to go.
        let err = stranger.send("lost").await.0.unwrap_err();
        assert_eq!(err.raw_os_error(), Some(libc::EDESTADDRREQ));
    });
}

#[test]
fn recv_from_is_cancel_safe_and_loses_no_datagram() {
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        let mut received = Vec::new();
        for round in 0..100u32 {
            // Most of these time out; whatever arrives while no call is
            // waiting must be handed to a later one.
            if round % 3 == 0 {
                a.send_to(round.to_be_bytes().to_vec(), b_addr)
                    .await
                    .0
                    .unwrap();
            }
            if let Ok(got) = timeout(Duration::from_micros(300), b.recv()).await {
                received.push(u32::from_be_bytes(got.unwrap()[..].try_into().unwrap()));
            }
        }
        while received.len() < 34 {
            let got = timeout(Duration::from_secs(5), b.recv()).await.unwrap();
            received.push(u32::from_be_bytes(got.unwrap()[..].try_into().unwrap()));
        }
        let expected: Vec<u32> = (0..100).filter(|r| r % 3 == 0).collect();
        assert_eq!(received, expected);
    });
}

#[test]
fn a_burst_arrives_complete_and_in_order() {
    // More datagrams than the receive backlog holds in pool buffers (8), sent
    // before the receiver looks: the rest wait on the heap or in the kernel.
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        // Arm the receive first, so the datagrams reach user space.
        assert!(timeout(Duration::from_millis(1), b.recv()).await.is_err());
        const COUNT: u32 = 64;
        for i in 0..COUNT {
            let mut data = payload(600, u64::from(i));
            data[..4].copy_from_slice(&i.to_be_bytes());
            a.send_to(data, b_addr).await.0.unwrap();
        }
        sleep(Duration::from_millis(30)).await;
        for i in 0..COUNT {
            let got = timeout(Duration::from_secs(5), b.recv())
                .await
                .unwrap()
                .unwrap();
            let mut expected = payload(600, u64::from(i));
            expected[..4].copy_from_slice(&i.to_be_bytes());
            assert!(*got == expected[..], "datagram {i}");
        }
        assert!(timeout(Duration::from_millis(20), b.recv()).await.is_err());
    });
}

#[test]
fn long_datagrams_are_truncated_by_recv_from_and_whole_with_read_from() {
    giong::block_on(async {
        let (a, a_addr) = socket();
        let (pooled, pooled_addr) = socket();
        let (owned, owned_addr) = socket();
        let data = payload(20_000, 3);

        // The pool's buffers are 8 KiB by default, 44 bytes of which hold the
        // sender's address.
        a.send_to(data.clone(), pooled_addr).await.0.unwrap();
        let (got, from) = pooled.recv_from().await.unwrap();
        assert!(got.is_truncated());
        assert_eq!((got.len(), from), (8192 - 44, a_addr));
        assert!(*got == data[..got.len()]);
        drop(got);
        // The longest datagram that fits is not truncated.
        a.send_to(data[..8192 - 44].to_vec(), pooled_addr)
            .await
            .0
            .unwrap();
        let got = pooled.recv().await.unwrap();
        assert!(!got.is_truncated() && *got == data[..8192 - 44]);

        // A buffer of one's own takes the whole datagram.
        a.send_to(data.clone(), owned_addr).await.0.unwrap();
        let (res, buf) = owned.read_from(Vec::with_capacity(65_536)).await;
        assert_eq!(res.unwrap(), (20_000, a_addr));
        assert!(buf == data);
        // The buffer is reused; a shorter one cuts the datagram off.
        a.send_to(data.clone(), owned_addr).await.0.unwrap();
        let (res, small) = owned.read_from(Vec::with_capacity(100)).await;
        assert_eq!(res.unwrap().0, 100);
        assert!(small == data[..100]);

        // The two ways of receiving do not mix.
        let (res, _) = pooled.read_from(buf).await;
        assert_eq!(res.unwrap_err().kind(), ErrorKind::InvalidInput);
    });
}

#[test]
fn larger_pool_buffers_carry_larger_datagrams() {
    giong::Builder::new()
        .recv_buffers(64, 65_536)
        .block_on(async {
            let (a, _) = socket();
            let (b, b_addr) = socket();
            let data = payload(60_000, 5);
            a.send_to(data.clone(), b_addr).await.0.unwrap();
            let got = b.recv().await.unwrap();
            assert!(!got.is_truncated() && *got == data[..]);
        })
        .unwrap();
}

#[test]
fn works_with_the_standard_library_on_the_other_side() {
    let std_socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let std_addr = std_socket.local_addr().unwrap();
    std_socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let giong_addr: SocketAddr = addr_rx.recv().unwrap();
        let mut buf = [0u8; 2048];
        for i in 0..100u32 {
            std_socket.send_to(&i.to_be_bytes(), giong_addr).unwrap();
            let (n, from) = std_socket.recv_from(&mut buf).unwrap();
            assert_eq!((from, &buf[..n]), (giong_addr, &(i + 1).to_be_bytes()[..]));
        }
    });
    giong::block_on(async {
        // A socket made by the standard library, handed to giong.
        let socket = UdpSocket::from_std(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
        addr_tx.send(socket.local_addr().unwrap()).unwrap();
        for _ in 0..100 {
            let (got, from) = socket.recv_from().await.unwrap();
            assert_eq!(from, std_addr);
            let n = u32::from_be_bytes(got[..].try_into().unwrap());
            drop(got);
            socket
                .send_to((n + 1).to_be_bytes().to_vec(), from)
                .await
                .0
                .unwrap();
        }
    });
    peer.join().unwrap();
}

#[test]
fn many_tasks_send_on_one_socket() {
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        let a = Rc::new(a);
        const TASKS: u32 = 50;
        let senders: Vec<_> = (0..TASKS)
            .map(|i| {
                let a = a.clone();
                giong::spawn(async move {
                    a.send_to(i.to_be_bytes().to_vec(), b_addr).await.0.unwrap();
                })
            })
            .collect();
        let mut seen = vec![false; TASKS as usize];
        for _ in 0..TASKS {
            let got = timeout(Duration::from_secs(5), b.recv())
                .await
                .unwrap()
                .unwrap();
            let i = u32::from_be_bytes(got[..].try_into().unwrap()) as usize;
            assert!(!std::mem::replace(&mut seen[i], true), "datagram {i} twice");
        }
        for sender in senders {
            sender.await.unwrap();
        }
    });
}

#[test]
fn every_worker_can_bind_the_same_port() {
    // Every UdpSocket sets SO_REUSEPORT. A socket that a test running at the
    // same time binds to 127.0.0.1 can get this port once the probe below has
    // let it go, and then joins the workers' group and takes some of their
    // datagrams. No other test binds to 127.0.0.2.
    const ADDR: &str = "127.0.0.2";
    // Reserve a port, then let three workers share it.
    let port = {
        let probe = std::net::UdpSocket::bind((ADDR, 0)).unwrap();
        probe.local_addr().unwrap().port()
    };
    let received = Arc::new(AtomicUsize::new(0));
    let bound = Arc::new(AtomicUsize::new(0));
    let rt = giong::Builder::new().workers(3).build().unwrap();
    let handle = rt.handle();
    let (r, b) = (received.clone(), bound.clone());
    let server = std::thread::spawn(move || {
        rt.run(move |_| {
            let (r, b) = (r.clone(), b.clone());
            async move {
                let socket = UdpSocket::bind((ADDR, port)).unwrap();
                b.fetch_add(1, Ordering::SeqCst);
                while let Some(Ok((got, from))) = giong::until_shutdown(socket.recv_from()).await {
                    let reply = got.to_vec();
                    drop(got);
                    socket.send_to(reply, from).await.0.unwrap();
                    r.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
        .unwrap();
    });
    while bound.load(Ordering::SeqCst) < 3 {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Many senders: the kernel picks a worker by the sender's address.
    const SENDERS: usize = 40;
    for i in 0..SENDERS {
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let message = format!("sender {i}");
        client.send_to(message.as_bytes(), (ADDR, port)).unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], message.as_bytes());
    }
    handle.shutdown();
    server.join().unwrap();
    assert_eq!(received.load(Ordering::SeqCst), SENDERS);
}

#[test]
fn an_unread_socket_does_not_pin_the_buffer_pool() {
    // Four pool buffers. A socket that nobody reads receives far more
    // datagrams than that; another socket must still get buffers, and the
    // unread datagrams must come out intact afterwards.
    giong::Builder::new()
        .recv_buffers(4, 512)
        .block_on(async {
            let (sender, sender_addr) = socket();
            let (stalled, stalled_addr) = socket();
            let (busy, busy_addr) = socket();
            assert!(timeout(Duration::from_millis(2), stalled.recv())
                .await
                .is_err());
            const COUNT: u32 = 40;
            for i in 0..COUNT {
                let mut data = payload(200, u64::from(i));
                data[..4].copy_from_slice(&i.to_be_bytes());
                sender.send_to(data, stalled_addr).await.0.unwrap();
            }
            sleep(Duration::from_millis(20)).await;

            for round in 0..50u8 {
                sender.send_to(vec![round; 100], busy_addr).await.0.unwrap();
                let got = timeout(Duration::from_secs(5), busy.recv())
                    .await
                    .expect("starved by a socket that is not being read")
                    .unwrap();
                assert!(got.len() == 100 && got.iter().all(|&b| b == round));
            }

            for i in 0..COUNT {
                let (got, from) = timeout(Duration::from_secs(5), stalled.recv_from())
                    .await
                    .unwrap()
                    .unwrap();
                let mut expected = payload(200, u64::from(i));
                expected[..4].copy_from_slice(&i.to_be_bytes());
                assert_eq!(from, sender_addr);
                assert!(*got == expected[..], "datagram {i}");
            }
            sleep(Duration::from_millis(20)).await;
            let stats = giong::stats();
            assert_eq!(stats.free_recv_buffers, 4, "{stats:?}");
        })
        .unwrap();
}

#[test]
fn runtime_teardown_with_datagrams_in_flight() {
    for _ in 0..20 {
        giong::block_on(async {
            let (a, _) = socket();
            let (b, b_addr) = socket();
            let (c, _) = socket();
            // Unread datagrams in the pool, a receive that never completes,
            // a one-off receive in flight, and a send that was never awaited.
            assert!(timeout(Duration::from_millis(1), b.recv()).await.is_err());
            for _ in 0..5 {
                a.send_to(vec![1u8; 300], b_addr).await.0.unwrap();
            }
            giong::spawn(async move {
                let _ = a.recv_from().await;
            });
            giong::spawn(async move {
                let _ = c.read_from(Vec::with_capacity(2048)).await;
            });
            let pending = b.send_to(vec![2u8; 100], b_addr);
            drop(pending);
            sleep(Duration::from_millis(2)).await;
        });
    }
}

/// The inode of a socket, to find out later whether any descriptor of this
/// process still refers to it. (Descriptor numbers are reused by the tests
/// running in parallel, inodes are not.)
fn socket_inode(socket: &UdpSocket) -> String {
    use std::os::fd::AsRawFd;
    let link = std::fs::read_link(format!("/proc/self/fd/{}", socket.as_raw_fd())).unwrap();
    link.to_string_lossy().into_owned()
}

fn is_open(inode: &str) -> bool {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .any(|target| target.to_string_lossy() == inode)
}

#[test]
fn dropping_a_socket_closes_it() {
    giong::block_on(async {
        // Never received on, received on with `read_from`, and with an armed
        // multishot receive: all three must give their descriptor back.
        let (idle, idle_addr) = socket();
        let (one_off, one_off_addr) = socket();
        let (multishot, multishot_addr) = socket();
        idle.send_to(&b"a"[..], one_off_addr).await.0.unwrap();
        idle.send_to(&b"b"[..], multishot_addr).await.0.unwrap();
        let (res, _) = one_off.read_from(Vec::with_capacity(16)).await;
        assert_eq!(res.unwrap(), (1, idle_addr));
        assert_eq!(&multishot.recv_from().await.unwrap().0[..], b"b");

        let inodes = [&idle, &one_off, &multishot].map(socket_inode);
        assert!(inodes.iter().all(|inode| is_open(inode)));
        drop((idle, one_off, multishot));
        sleep(Duration::from_millis(20)).await;
        for inode in &inodes {
            assert!(!is_open(inode), "{inode} is still open");
        }
    });
}

#[test]
fn a_cancelled_read_from_leaves_the_socket_usable() {
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        for _ in 0..50 {
            let read = b.read_from(Vec::with_capacity(512));
            assert!(timeout(Duration::from_micros(300), read).await.is_err());
        }
        sleep(Duration::from_millis(20)).await;
        assert_eq!(
            giong::stats().tracked_ops,
            0,
            "cancelled reads were not cleaned up"
        );

        a.send_to(&b"still here"[..], b_addr).await.0.unwrap();
        let read = b.read_from(Vec::with_capacity(512));
        let (res, buf) = timeout(Duration::from_secs(30), read).await.unwrap();
        assert_eq!(res.unwrap().0, 10);
        assert_eq!(buf, b"still here");
    });
}

#[test]
fn one_buffer_echoes_many_datagrams_between_tasks() {
    const N: u32 = 2_000;
    giong::block_on(async {
        let (server, server_addr) = socket();
        let echo = giong::spawn(async move {
            // The buffer goes into each operation by value and comes back.
            let mut buf = Vec::with_capacity(64);
            for _ in 0..N {
                let (res, b) = server.read_from(buf).await;
                let (_, peer) = res.unwrap();
                let (res, b) = server.send_to(b, peer).await;
                res.unwrap();
                buf = b;
            }
        });
        let (client, _) = socket();
        client.connect(server_addr).unwrap();
        for i in 0..N {
            client.send(i.to_le_bytes().to_vec()).await.0.unwrap();
            let read = client.read(Vec::with_capacity(64));
            let (res, buf) = timeout(Duration::from_secs(30), read)
                .await
                .expect("datagram lost on loopback");
            assert_eq!(res.unwrap(), 4);
            assert_eq!(buf, i.to_le_bytes());
        }
        echo.await.unwrap();
    });
}
