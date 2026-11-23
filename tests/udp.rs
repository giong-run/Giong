//! UDP: send_to, recv_from (multishot + provided buffers), read_from.

// Some tests and their helpers are Unix-only for now.
#![cfg_attr(windows, allow(dead_code, unused_imports))]

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

mod common;

use giong::net::UdpSocket;
use giong::time::{sleep, timeout};

fn socket() -> (UdpSocket, SocketAddr) {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    (socket, addr)
}

/// Lets this process open at least `needed` descriptors, by raising the soft
/// limit (often 1024) towards the hard one.
#[cfg(unix)]
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
        lim.rlim_cur = needed;
        assert_eq!(
            libc::setrlimit(libc::RLIMIT_NOFILE, &lim),
            0,
            "{}",
            std::io::Error::last_os_error()
        );
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
fn read_from_refuses_a_buffer_with_no_room_and_keeps_the_datagram() {
    // Vec::new() has no capacity: the datagram used to be received into it
    // and lost, with Ok(0) as the only sign.
    giong::block_on(async {
        let (a, a_addr) = socket();
        let (b, b_addr) = socket();
        a.send_to("hello", b_addr).await.0.unwrap();
        sleep(Duration::from_millis(5)).await;
        let (res, _) = b.read_from(Vec::new()).await;
        let err = res.unwrap_err();
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::EmptyBuffer),
            "{err}"
        );
        let (res, buf) = timeout(Duration::from_secs(5), b.read_from(Vec::with_capacity(64)))
            .await
            .expect("the datagram was lost");
        assert_eq!(res.unwrap(), (5, a_addr));
        assert_eq!(buf, b"hello");
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
        assert_eq!(err.raw_os_error(), Some(common::errno::NO_PEER));
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
    // More datagrams than the receive backlog holds in pool buffers (an
    // eighth of a pool of 64, so 8), sent before the receiver looks, with the
    // pool running low: the rest wait on the heap or in the kernel.
    giong::Builder::new()
        .recv_buffers(64, 8192)
        .block_on(async {
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
        })
        .unwrap();
}

#[test]
fn long_datagrams_are_truncated_by_recv_from_and_whole_with_read_from() {
    giong::block_on(async {
        let (a, a_addr) = socket();
        let (pooled, pooled_addr) = socket();
        let (owned, owned_addr) = socket();
        let data = payload(20_000, 3);

        // The pool's buffers are 16 KiB by default, 44 bytes of which hold the
        // sender's address.
        a.send_to(data.clone(), pooled_addr).await.0.unwrap();
        let (got, from) = pooled.recv_from().await.unwrap();
        assert!(got.is_truncated());
        assert_eq!((got.len(), from), (16384 - 44, a_addr));
        assert!(*got == data[..got.len()]);
        drop(got);
        // The longest datagram that fits is not truncated.
        a.send_to(data[..16384 - 44].to_vec(), pooled_addr)
            .await
            .0
            .unwrap();
        let got = pooled.recv().await.unwrap();
        assert!(!got.is_truncated() && *got == data[..16384 - 44]);

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
fn a_datagram_already_waiting_and_too_long_for_the_buffer_is_cut_safely() {
    // The datagram is there before the receive starts, so the receive ends
    // inside the call, cut off. On Windows that is a warning status, which
    // still reports through the completion port: the receive's record must
    // live until then, while the worker goes on with other I/O.
    giong::block_on(async {
        let (a, a_addr) = socket();
        let (b, b_addr) = socket();
        let data = payload(2000, 7);
        let mut buf = Vec::with_capacity(512);
        for round in 0..200 {
            a.send_to(data.clone(), b_addr).await.0.unwrap();
            // Let the datagram arrive first.
            sleep(Duration::from_millis(1)).await;
            let (res, got) = b.read_from(buf).await;
            assert_eq!(res.unwrap(), (512, a_addr), "round {round}");
            assert!(got == data[..512], "round {round}");
            buf = got;
            // Other I/O on the worker meanwhile, in both directions.
            b.send_to(vec![round as u8; 64], a_addr).await.0.unwrap();
            let (back, from) = a.recv_from().await.unwrap();
            assert_eq!((back.len(), back[0], from), (64, round as u8, b_addr));
        }
    });
}

#[test]
fn a_send_to_a_peer_that_has_gone_does_not_fail_later_receives() {
    // The peer's port is closed, so the system answers the datagram with
    // an ICMP "port unreachable". Unix reports that only on a connected
    // socket; on Windows it would fail the next receive with a reset, and
    // a server's receive loop would end there.
    giong::block_on(async {
        let (server, server_addr) = socket();
        let (client, client_addr) = socket();
        let gone = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let gone_addr = gone.local_addr().unwrap();
        drop(gone);
        for _ in 0..3 {
            server
                .send_to(b"late reply".to_vec(), gone_addr)
                .await
                .0
                .unwrap();
        }
        sleep(Duration::from_millis(50)).await;
        client
            .send_to(b"next".to_vec(), server_addr)
            .await
            .0
            .unwrap();
        let (got, from) = timeout(Duration::from_secs(5), server.recv_from())
            .await
            .expect("the datagram arrives")
            .expect("the receive goes on");
        assert_eq!((&*got, from), (&b"next"[..], client_addr));
        // And with a buffer of one's own.
        let (server, server_addr) = socket();
        for _ in 0..3 {
            server
                .send_to(b"late reply".to_vec(), gone_addr)
                .await
                .0
                .unwrap();
        }
        sleep(Duration::from_millis(50)).await;
        client
            .send_to(b"again".to_vec(), server_addr)
            .await
            .0
            .unwrap();
        let (res, buf) = server.read_from(Vec::with_capacity(64)).await;
        assert_eq!(res.unwrap(), (5, client_addr));
        assert_eq!(buf, b"again");
    });
}

/// The sends that do not wait rely on the socket being non-blocking from
/// the start, before the driver has seen it: on Windows, where no send has
/// a flag that says so. (One name on every system, for the simulator's
/// index; the body is Windows'.)
#[test]
fn a_new_socket_is_non_blocking_before_its_first_operation() {
    #[cfg(windows)]
    giong::block_on(async {
        use std::os::windows::io::AsRawSocket;
        let (sock, _) = socket();
        let mut byte = [0u8; 1];
        // A blocking socket would wait here for a datagram that never comes.
        // SAFETY: an open socket and a local buffer of the size given.
        let n = unsafe {
            windows_sys::Win32::Networking::WinSock::recv(
                sock.as_raw_socket() as _,
                byte.as_mut_ptr(),
                1,
                0,
            )
        };
        assert_eq!(n, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(10035), // WSAEWOULDBLOCK
        );
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
fn many_tasks_receive_on_one_socket() {
    // Each datagram wakes a waiting task of its own. Before 0.9 the tasks
    // shared one wake-up slot on io_uring: only the last task to wait was
    // woken, and the others never saw the datagrams queued for them.
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        let b = Rc::new(b);
        const TASKS: u32 = 8;
        let receivers: Vec<_> = (0..TASKS)
            .map(|_| {
                let b = b.clone();
                giong::spawn(async move {
                    let got = timeout(Duration::from_secs(5), b.recv())
                        .await
                        .expect("a waiting task was never woken")
                        .unwrap();
                    u32::from_be_bytes(got[..].try_into().unwrap())
                })
            })
            .collect();
        // Let every task start waiting.
        sleep(Duration::from_millis(20)).await;
        for i in 0..TASKS {
            a.send_to(i.to_be_bytes().to_vec(), b_addr).await.0.unwrap();
        }
        let sent = std::time::Instant::now();
        let mut seen = vec![false; TASKS as usize];
        for r in receivers {
            let i = r.await.unwrap() as usize;
            assert!(!std::mem::replace(&mut seen[i], true), "datagram {i} twice");
        }
        // A task that is not woken still gets its datagram when its timeout
        // polls it at the deadline: check that nobody waited for that.
        let waited = sent.elapsed();
        assert!(
            waited < Duration::from_secs(2),
            "tasks were woken late: {waited:?}"
        );
    });
}

#[test]
fn a_waiting_task_gets_a_datagram_another_receive_took_from_the_socket() {
    // A task waits on a socket; datagrams arrive, and before the event loop
    // looks, another task's receives take them all out of the socket, the
    // last of them in one system call with a datagram more than that task
    // asks for (the epoll driver receives several at once on a busy
    // socket). That datagram must go to the waiting task: the socket is
    // empty by then, so no readiness event would ever wake it.
    giong::block_on(async {
        let (b, b_addr) = socket();
        let b = Rc::new(b);
        let a = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let waiter = {
            let b = b.clone();
            giong::spawn(async move { b.recv().await.unwrap().to_vec() })
        };
        // Let it start waiting.
        sleep(Duration::from_millis(20)).await;
        for i in 0u32..4 {
            a.send_to(&i.to_be_bytes(), b_addr).unwrap();
        }
        // Three receives without a turn of the event loop in between: the
        // receive batch grows from one datagram to two on the third.
        let mut seen = vec![false; 4];
        for _ in 0..3 {
            let got = b.recv().await.unwrap();
            seen[u32::from_be_bytes(got[..].try_into().unwrap()) as usize] = true;
        }
        let got = timeout(Duration::from_secs(2), waiter)
            .await
            .expect("the waiting task was never woken")
            .unwrap();
        seen[u32::from_be_bytes(got[..].try_into().unwrap()) as usize] = true;
        assert!(seen.iter().all(|&s| s), "{seen:?}");
    });
}

#[test]
fn a_receiver_that_gave_up_does_not_take_the_next_datagram() {
    // A task stops waiting (its receive timed out); the next datagram must
    // wake the task that is waiting now, not the one that left.
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        let b = Rc::new(b);
        let b1 = b.clone();
        giong::spawn(async move {
            assert!(timeout(Duration::from_millis(10), b1.recv()).await.is_err());
            // Keep this task alive, so that a wake-up meant for the other
            // one would land here.
            sleep(Duration::from_secs(10)).await;
        });
        sleep(Duration::from_millis(30)).await;
        let b2 = b.clone();
        let waiting = giong::spawn(async move {
            timeout(Duration::from_secs(5), b2.recv())
                .await
                .expect("the waiting task was not woken")
                .unwrap()
                .to_vec()
        });
        sleep(Duration::from_millis(10)).await;
        let sent = std::time::Instant::now();
        a.send_to(&b"next"[..], b_addr).await.0.unwrap();
        assert_eq!(waiting.await.unwrap(), b"next");
        let waited = sent.elapsed();
        assert!(
            waited < Duration::from_secs(2),
            "the task was woken late: {waited:?}"
        );
    });
}

#[test]
fn a_task_that_only_sends_lets_the_others_run() {
    // Sends of datagrams complete at once, so a task that does nothing else
    // never waits; the runtime must still make it give way now and then.
    giong::block_on(async {
        let (a, _) = socket();
        let (_b, b_addr) = socket();
        let ran = Rc::new(std::cell::Cell::new(false));
        let r = ran.clone();
        giong::spawn(async move { r.set(true) });
        let mut first_seen = None;
        for i in 0..5_000u32 {
            a.send_to(&b"x"[..], b_addr).await.0.unwrap();
            if ran.get() {
                first_seen = Some(i);
                break;
            }
        }
        let i = first_seen.expect("the other task never ran during 5000 sends");
        assert!(i < 1_000, "the other task first ran after {i} sends");
    });
}

#[test]
fn every_worker_can_bind_the_same_port() {
    // No other test binds to 127.0.0.2, and the port is one the kernel never
    // hands out for port 0: a socket of a test running at the same time,
    // bound to port 0 of any address (0.0.0.0 included), cannot take it
    // before the workers bind it.
    // macOS has 127.0.0.1 only, unless more are added to the interface.
    const LAST: u8 = if cfg!(target_os = "macos") { 1 } else { 2 };
    let ip = std::net::Ipv4Addr::new(127, 0, 0, LAST);
    let port = port_outside_ephemeral_range(ip.octets()).port();
    let received = Arc::new(AtomicUsize::new(0));
    let bound = Arc::new(AtomicUsize::new(0));
    let rt = giong::Builder::new().workers(3).build().unwrap();
    let handle = rt.handle();
    let (r, b) = (received.clone(), bound.clone());
    let server = std::thread::spawn(move || {
        rt.run(move |_| {
            let (r, b) = (r.clone(), b.clone());
            async move {
                let socket = UdpSocket::bind((ip, port)).unwrap();
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
        client.send_to(message.as_bytes(), (ip, port)).unwrap();
        let mut buf = [0u8; 64];
        let (n, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], message.as_bytes());
    }
    handle.shutdown();
    server.join().unwrap();
    assert_eq!(received.load(Ordering::SeqCst), SENDERS);
}

#[test]
fn a_shared_port_keeps_each_sender_on_one_socket() {
    // The socket that binds first receives through `recv_from`.
    shared_port_keeps_senders_apart([127, 0, 0, 3], 0);
}

#[test]
fn a_shared_port_keeps_each_sender_on_one_socket_with_read_from() {
    // The socket that binds first receives through `read_from`.
    shared_port_keeps_senders_apart([127, 0, 0, 4], 1);
}

/// Each worker's socket answers with the order in which it was bound, half
/// of them through `recv_from` and half through `read_from` (those whose
/// order has the parity `read_from_parity`). Every datagram of one sender
/// must reach the same socket, and the senders must not all reach one.
/// Afterwards the port is free again.
fn shared_port_keeps_senders_apart(ip: [u8; 4], read_from_parity: usize) {
    // macOS answers only on 127.0.0.1 unless told otherwise, and keeps its
    // port range elsewhere than /proc.
    if cfg!(target_os = "macos") {
        return;
    }
    const WORKERS: usize = 4;
    let addr = port_outside_ephemeral_range(ip);
    let bound = Arc::new(AtomicUsize::new(0));
    let rt = giong::Builder::new().workers(WORKERS).build().unwrap();
    let handle = rt.handle();
    let b = bound.clone();
    let server = std::thread::spawn(move || {
        rt.run(move |_| {
            let b = b.clone();
            async move {
                let socket = UdpSocket::bind(addr).unwrap();
                let index = b.fetch_add(1, Ordering::SeqCst);
                loop {
                    let from = if index % 2 != read_from_parity {
                        match giong::until_shutdown(socket.recv_from()).await {
                            Some(Ok((_, from))) => from,
                            _ => break,
                        }
                    } else {
                        let read = socket.read_from(Vec::with_capacity(64));
                        match giong::until_shutdown(read).await {
                            Some((Ok((_, from)), _)) => from,
                            _ => break,
                        }
                    };
                    let reply = vec![index as u8];
                    socket.send_to(reply, from).await.0.unwrap();
                }
            }
        })
        .unwrap();
    });
    while bound.load(Ordering::SeqCst) < WORKERS {
        std::thread::sleep(Duration::from_millis(1));
    }
    const SENDERS: usize = 40;
    let mut reached = std::collections::HashSet::new();
    for i in 0..SENDERS {
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut seen = None;
        for _ in 0..5 {
            client.send_to(b"which", addr).unwrap();
            let mut buf = [0u8; 8];
            let (n, _) = client.recv_from(&mut buf).unwrap();
            assert_eq!(n, 1);
            let socket = buf[0];
            assert_eq!(
                *seen.get_or_insert(socket),
                socket,
                "sender {i} reached two sockets"
            );
        }
        reached.extend(seen);
    }
    assert!(
        reached.len() > 1,
        "{SENDERS} senders all reached one socket"
    );
    handle.shutdown();
    server.join().unwrap();
    std::net::UdpSocket::bind(addr).expect("the port is still held");
}

#[cfg(unix)]
#[test]
fn port_zero_gives_every_socket_a_port_of_its_own() {
    // With SO_REUSEPORT set before binding to port 0, Linux picks ports
    // without regard to other sockets of this user that hold them with the
    // same option, and puts the new socket in their group: of 1,000 sockets,
    // some 15 pairs shared a port, and each pair split its datagrams.
    const N: usize = 1_000;
    allow_open_files(N as u64 + 1024);
    giong::block_on(async {
        let sockets: Vec<_> = (0..N).map(|_| socket()).collect();
        let mut ports: Vec<u16> = sockets.iter().map(|(_, addr)| addr.port()).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), N, "two sockets bound to port 0 share a port");

        // Nor can a socket join such a port later.
        let (_, addr) = &sockets[0];
        let err = UdpSocket::bind(addr).expect_err("joined a port-0 socket");
        assert_eq!(err.kind(), ErrorKind::AddrInUse);
    });
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
            let stats = giong::observe::worker_stats();
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

/// What names one socket for good: its address and its cookie, a number the
/// kernel never gives another socket. (Descriptor numbers are reused by the
/// tests running in parallel.)
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug)]
struct SocketId {
    addr: std::net::SocketAddrV4,
    cookie: u64,
    inode: u64,
}

#[cfg(target_os = "linux")]
fn socket_id(socket: &UdpSocket) -> SocketId {
    use std::os::fd::AsRawFd;
    const SO_COOKIE: libc::c_int = 57;
    let mut cookie = 0u64;
    let mut len = std::mem::size_of::<u64>() as libc::socklen_t;
    // SAFETY: `cookie` and `len` are valid for the kernel to write.
    let rc = unsafe {
        libc::getsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            SO_COOKIE,
            (&raw mut cookie).cast(),
            &mut len,
        )
    };
    assert_eq!(rc, 0, "SO_COOKIE: {}", std::io::Error::last_os_error());
    let std::net::SocketAddr::V4(addr) = socket.local_addr().unwrap() else {
        panic!("not an IPv4 socket");
    };
    // SAFETY: `socket` keeps the descriptor open while it is borrowed here.
    let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(socket.as_raw_fd()) };
    let file = std::fs::File::from(fd.try_clone_to_owned().unwrap());
    let inode = std::os::unix::fs::MetadataExt::ino(&file.metadata().unwrap());
    SocketId {
        addr,
        cookie,
        inode,
    }
}

/// Whether the socket `id` still exists.
///
/// Not from /proc/self/fd: listing or reading those links holds every open
/// file for a moment, and a socket another test closes in that moment keeps
/// its port until this thread is done (the rebind test then saw AddrInUse
/// for a port nothing held). The kernel is asked by address and cookie
/// instead (sock_diag), in one request. Kernels that cannot answer for UDP
/// (its sock_diag handler is a module, `udp_diag`, which the QEMU guests of
/// CI do not have) fall back to the UDP table; the kernel writes that a page
/// per read, and sockets that come and go in between (the thousand of
/// `port_zero_gives_every_socket_a_port_of_its_own`) can make a live socket
/// fall between two pages, so it counts as open when any of several reads
/// lists it.
#[cfg(target_os = "linux")]
fn is_open(id: SocketId) -> bool {
    static DIAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let diag = *DIAG.get_or_init(|| {
        let probe = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let probe = UdpSocket::from_std(probe);
        found_by_sock_diag(socket_id(&probe))
    });
    if diag {
        return found_by_sock_diag(id);
    }
    let inode = id.inode.to_string();
    (0..20).any(|_| {
        ["/proc/net/udp", "/proc/net/udp6"].iter().any(|table| {
            std::fs::read_to_string(table)
                .unwrap_or_default()
                .lines()
                .skip(1)
                .any(|line| line.split_whitespace().nth(9) == Some(inode.as_str()))
        })
    })
}

/// Whether sock_diag finds the UDP socket bound to `id.addr` with
/// `id.cookie`. Also false when the kernel has no sock_diag for UDP.
#[cfg(target_os = "linux")]
fn found_by_sock_diag(id: SocketId) -> bool {
    #[repr(C)]
    struct Request {
        len: u32,
        kind: u16,
        flags: u16,
        seq: u32,
        pid: u32,
        family: u8,
        protocol: u8,
        ext: u8,
        pad: u8,
        states: u32,
        sport: [u8; 2],
        dport: [u8; 2],
        src: [u8; 16],
        dst: [u8; 16],
        interface: u32,
        cookie: [u32; 2],
    }
    const NETLINK_SOCK_DIAG: libc::c_int = 4;
    const SOCK_DIAG_BY_FAMILY: u16 = 20;
    let mut src = [0u8; 16];
    src[..4].copy_from_slice(&id.addr.ip().octets());
    // A datagram from 127.0.0.1:1 to the socket's address finds it.
    let mut from = [0u8; 16];
    from[..4].copy_from_slice(&[127, 0, 0, 1]);
    let request = Request {
        len: std::mem::size_of::<Request>() as u32,
        kind: SOCK_DIAG_BY_FAMILY,
        flags: libc::NLM_F_REQUEST as u16,
        seq: 1,
        pid: 0,
        family: libc::AF_INET as u8,
        protocol: libc::IPPROTO_UDP as u8,
        ext: 0,
        pad: 0,
        states: u32::MAX,
        sport: 1u16.to_be_bytes(),
        dport: id.addr.port().to_be_bytes(),
        src: from,
        dst: src,
        interface: 0,
        cookie: [id.cookie as u32, (id.cookie >> 32) as u32],
    };
    // SAFETY: plain system calls on a socket this function owns, with
    // buffers that outlive them.
    unsafe {
        let fd = libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            NETLINK_SOCK_DIAG,
        );
        assert!(fd >= 0, "netlink: {}", std::io::Error::last_os_error());
        let fd = std::os::fd::OwnedFd::from_raw_fd(fd);
        use std::os::fd::{AsRawFd, FromRawFd};
        let sent = libc::send(
            fd.as_raw_fd(),
            (&raw const request).cast(),
            std::mem::size_of::<Request>(),
            0,
        );
        assert_eq!(sent, std::mem::size_of::<Request>() as isize);
        let mut reply = [0u32; 1024];
        let got = libc::recv(fd.as_raw_fd(), reply.as_mut_ptr().cast(), 4096, 0);
        assert!(
            got >= 20,
            "sock_diag reply: {}",
            std::io::Error::last_os_error()
        );
        // The header's type: the socket's description when it was found, an
        // error when it was not: `ENOENT` when no socket has the address,
        // `ESTALE` when another one does (a test running in parallel was
        // given the port once this socket let it go; port-0 sockets do not
        // share ports, so this one is closed).
        let kind = reply[1] as u16;
        if kind == SOCK_DIAG_BY_FAMILY {
            return true;
        }
        assert_eq!(kind, libc::NLMSG_ERROR as u16);
        let errno = -(reply[4] as i32);
        assert!(
            errno == libc::ENOENT || errno == libc::ESTALE,
            "sock_diag: {}",
            std::io::Error::from_raw_os_error(errno)
        );
        false
    }
}

// Linux only: the sockets are found through sock_diag or /proc/net/udp.
#[cfg(target_os = "linux")]
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

        let ids = [&idle, &one_off, &multishot].map(socket_id);
        assert!(ids.iter().all(|&id| is_open(id)));
        drop((idle, one_off, multishot));
        sleep(Duration::from_millis(20)).await;
        for id in ids {
            assert!(!is_open(id), "{id:?} is still open");
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
            giong::observe::worker_stats().tracked_ops,
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

/// A `read_from` given up on leaves a cancellation queued for the kernel. A
/// datagram sent right after it, and answered at once, must reach the next
/// `read_from`, not the receive being cancelled: a send made within the call
/// must not overtake the cancellation (seen with Linux 6.0's io_uring, which
/// runs completions before the next submission).
#[test]
fn a_reply_to_a_send_after_a_cancelled_read_from_is_not_lost() {
    reply_after_cancelled_read_from(false);
}

/// The same with another task running, so that the send waits for the end
/// of the turn and goes out with `sendmmsg(2)`.
#[test]
fn a_reply_to_a_batched_send_after_a_cancelled_read_from_is_not_lost() {
    reply_after_cancelled_read_from(true);
}

fn reply_after_cancelled_read_from(busy: bool) {
    let echo = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let echo_addr = echo.local_addr().unwrap();
    // Echoes until it gets an empty datagram.
    let peer = std::thread::spawn(move || {
        let mut buf = [0u8; 64];
        loop {
            let (n, from) = echo.recv_from(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            echo.send_to(&buf[..n], from).unwrap();
        }
    });
    giong::block_on(async {
        let running = Rc::new(std::cell::Cell::new(true));
        if busy {
            let running = running.clone();
            giong::spawn(async move {
                while running.get() {
                    giong::yield_now().await;
                }
            });
        }
        let (a, _) = socket();
        for i in 0..200u32 {
            let read = a.read_from(Vec::with_capacity(64));
            assert!(timeout(Duration::from_micros(200), read).await.is_err());
            a.send_to(i.to_be_bytes().to_vec(), echo_addr)
                .await
                .0
                .unwrap();
            let read = a.read_from(Vec::with_capacity(64));
            let (res, buf) = timeout(Duration::from_secs(5), read)
                .await
                .unwrap_or_else(|_| panic!("the reply to datagram {i} was lost"));
            assert_eq!(res.unwrap().0, 4);
            assert_eq!(buf, i.to_be_bytes());
        }
        running.set(false);
        a.send_to(Vec::new(), echo_addr).await.0.unwrap();
    });
    peer.join().unwrap();
}

#[cfg(unix)]
#[test]
fn a_new_socket_with_a_closed_sockets_number_is_left_alone() {
    // A receive that the runtime has queued but not yet handed to the kernel
    // names its socket by number. Once the socket is closed, a new socket can
    // get that number at once; the queued receive must not take its data.
    use std::future::Future;
    use std::os::fd::AsRawFd;
    use std::task::Poll;
    giong::block_on(async {
        let (old, _) = socket();
        let mut recv = Box::pin(old.recv());
        std::future::poll_fn(|cx| {
            assert!(recv.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        drop(recv);
        let number = old.as_raw_fd();
        drop(old);

        // Opened before the runtime turns again, so it gets the lowest free
        // number: usually the old socket's, unless a test running alongside
        // took it first.
        let new = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"mine", new.local_addr().unwrap()).unwrap();
        sleep(Duration::from_millis(20)).await;

        new.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 16];
        let n = new.recv(&mut buf).unwrap_or_else(|e| {
            let reused = new.as_raw_fd() == number;
            panic!("the datagram is gone ({e}); descriptor number reused: {reused}")
        });
        assert_eq!(&buf[..n], b"mine");
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

/// Another process's UDP socket on a port, whatever options it set, keeps
/// giong's sockets off that port: with `SO_REUSEPORT` the kernel would share
/// its datagrams with them, and with `SO_REUSEADDR` on both sides (which
/// giong no longer sets) any user could.
#[cfg(unix)]
#[test]
fn a_port_held_by_another_process_is_refused() {
    giong::block_on(async {
        for (reuse_addr, reuse_port) in [(false, true), (true, false), (true, true), (false, false)]
        {
            let other =
                socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
            other.set_reuse_address(reuse_addr).unwrap();
            other.set_reuse_port(reuse_port).unwrap();
            other
                .bind(&std::net::SocketAddr::from(([127, 0, 0, 1], 0)).into())
                .unwrap();
            let addr = other.local_addr().unwrap().as_socket().unwrap();
            let err = UdpSocket::bind(addr).expect_err("joined another process's socket");
            assert_eq!(
                err.kind(),
                ErrorKind::AddrInUse,
                "{reuse_addr} {reuse_port}: {err}"
            );
        }
    });
}

/// A UDP port on `ip` (a loopback address) that is free now and that the
/// kernel never hands out for port 0, so the other tests of this binary,
/// which bind port 0 in parallel, cannot take it while a test has let go of
/// it. Picked at random
/// below the ephemeral range rather than always the highest such port, which
/// is the obvious pick for anything else on the machine doing the same.
fn port_outside_ephemeral_range(ip: [u8; 4]) -> std::net::SocketAddr {
    use std::hash::{BuildHasher, Hasher};
    #[cfg(target_os = "linux")]
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    #[cfg(target_os = "macos")]
    let range = {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "net.inet.ip.portrange.first"])
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    // Windows hands out ports from 49152 on unless told otherwise.
    #[cfg(windows)]
    let range = String::from("49152");
    let low: u16 = range.split_whitespace().next().unwrap().parse().unwrap();
    assert!(
        low > 2048,
        "ephemeral range starts at {low}: no room below it"
    );
    let mut seed = std::collections::hash_map::RandomState::new().build_hasher();
    seed.write_u32(std::process::id());
    let start = seed.finish();
    let span = u64::from(low - 1024);
    (0..span)
        .map(|i| 1024 + ((start + i) % span) as u16)
        .map(|port| std::net::SocketAddr::from((ip, port)))
        .find(|addr| std::net::UdpSocket::bind(addr).is_ok())
        .expect("no free UDP port below the ephemeral range")
}

/// The process that owns socket `inode`, when /proc lets this process see
/// it (processes of other users hide their descriptors). Only for a report
/// after a failure: reading /proc/*/fd holds sockets that close meanwhile
/// (see `is_open`).
fn socket_owner(inode: &str) -> Option<String> {
    let target = format!("socket:[{inode}]");
    for proc in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid = proc.file_name().to_string_lossy().into_owned();
        if !pid.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(proc.path().join("fd")) else {
            continue;
        };
        let owns = fds
            .flatten()
            .filter_map(|fd| std::fs::read_link(fd.path()).ok())
            .any(|link| link.to_string_lossy() == target);
        if owns {
            let cmdline = std::fs::read(proc.path().join("cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            let me = if pid == std::process::id().to_string() {
                " (this process)"
            } else {
                ""
            };
            return Some(format!("pid {pid}{me}: {}", cmdline.trim()));
        }
    }
    None
}

/// Every UDP socket on `port` in the system, by inode: local and remote
/// address, uid, and the owning process when it can be seen.
fn udp_holders(port: u16) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for table in ["/proc/net/udp", "/proc/net/udp6"] {
        let text = std::fs::read_to_string(table).unwrap_or_default();
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            let Some(local) = f.get(1) else { continue };
            let hex_port = local.rsplit(':').next().unwrap_or("");
            if u16::from_str_radix(hex_port, 16).ok() != Some(port) {
                continue;
            }
            let inode = f.get(9).copied().unwrap_or("?");
            let holder = format!(
                "{table}: local {local} remote {} uid {}, {}",
                f.get(2).copied().unwrap_or("?"),
                f.get(7).copied().unwrap_or("?"),
                socket_owner(inode).unwrap_or_else(|| "owner gone or not visible".into())
            );
            out.push((inode.to_string(), holder));
        }
    }
    out
}

/// Which plain binds of `addr` the kernel refuses now: none of the options,
/// `SO_REUSEADDR` only (giong's probe after a release) and `SO_REUSEPORT`
/// only (giong's own sockets).
#[cfg(unix)]
fn refused_binds(addr: std::net::SocketAddr) -> String {
    let try_bind = |reuse_addr: bool, reuse_port: bool| {
        let sock = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::DGRAM, None).unwrap();
        sock.set_reuse_address(reuse_addr).unwrap();
        sock.set_reuse_port(reuse_port).unwrap();
        match sock.bind(&addr.into()) {
            Ok(()) => "free",
            Err(_) => "refused",
        }
    };
    format!(
        "no options {}, SO_REUSEADDR {}, SO_REUSEPORT {}",
        try_bind(false, false),
        try_bind(true, false),
        try_bind(false, true)
    )
}

/// What a failed bind of `addr` ran into. A socket of another process that
/// lives only for a moment is gone before a single look, so this looks for
/// half a second and reports every socket seen on the port, when it was
/// first seen, and which binds the port refused at each change.
#[cfg(unix)]
fn bind_failure_report(addr: std::net::SocketAddr) -> String {
    let start = std::time::Instant::now();
    let mut seen: Vec<String> = Vec::new();
    let mut seen_inodes: Vec<String> = Vec::new();
    // Which binds the port refuses right after the failure; later it
    // changes hands.
    let mut report = format!("\n  binds now: {}", refused_binds(addr));
    while start.elapsed() < Duration::from_millis(500) {
        let at = start.elapsed();
        for (inode, holder) in udp_holders(addr.port()) {
            if seen_inodes.contains(&inode) {
                continue;
            }
            seen_inodes.push(inode);
            // The same kind of socket again (another process binding the
            // port over and over) is counted, not listed again.
            if !seen.contains(&holder) && seen.len() < 10 {
                report.push_str(&format!("\n  +{at:?} socket {holder}"));
                seen.push(holder);
            }
        }
        std::thread::sleep(Duration::from_micros(200));
    }
    if seen.is_empty() {
        report.push_str("\n  no socket on the port was seen in 500 ms");
    } else {
        report.push_str(&format!(
            "\n  {} sockets on the port in 500 ms",
            seen_inodes.len()
        ));
    }
    report
}

/// Workers of one process share a port; a dropped socket's port can be
/// bound again at once.
#[cfg(unix)]
#[test]
fn sockets_of_this_process_share_a_port_and_rebind_at_once() {
    giong::block_on(async {
        // Not a port from port 0: between the drops and the binds below,
        // another test's socket could be given the same port (seen in CI).
        let addr = port_outside_ephemeral_range([127, 0, 0, 1]);
        let first = UdpSocket::bind(addr).unwrap();
        let second = UdpSocket::bind(addr).unwrap();
        drop(first);
        drop(second);
        for round in 0..20 {
            let again = UdpSocket::bind(addr).unwrap_or_else(|e| {
                panic!(
                    "round {round}: binding {addr} again failed: {e}; on the port since:{}",
                    bind_failure_report(addr)
                )
            });
            let (sender, _) = socket();
            sender.send_to(vec![7; 10], addr).await.0.unwrap();
            let got = timeout(Duration::from_secs(5), again.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&*got, &[7; 10]);
        }
    });
}

#[test]
fn bind_and_connect_refuse_a_host_name() {
    giong::block_on(async {
        let err = UdpSocket::bind("localhost:5353").expect_err("bound a host name");
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::InvalidAddress),
            "{err}"
        );
        let (s, _) = socket();
        let err = s
            .connect("localhost:53")
            .expect_err("connected to a host name");
        assert_eq!(
            giong::error::code(&err),
            Some(giong::error::Code::InvalidAddress),
            "{err}"
        );
        s.connect("127.0.0.1:53").unwrap();
    });
}

#[test]
fn datagrams_sent_in_one_turn_each_get_their_own_result() {
    // Tasks that send in the same turn of the loop go to the kernel in one
    // batch; each still gets its own answer: the oversized datagram fails
    // alone, the others arrive.
    giong::block_on(async {
        let a = Rc::new(socket().0);
        let (b, b_addr) = socket();
        let senders: Vec<_> = (0..8u32)
            .map(|i| {
                let a = a.clone();
                giong::spawn(async move {
                    let buf = if i == 3 {
                        vec![0u8; 70_000]
                    } else {
                        i.to_be_bytes().to_vec()
                    };
                    let len = buf.len();
                    (i, len, a.send_to(buf, b_addr).await.0)
                })
            })
            .collect();
        for s in senders {
            let (i, len, res) = s.await.unwrap();
            if i == 3 {
                let e = res.expect_err("a 70 000-byte datagram cannot be sent");
                assert_eq!(e.raw_os_error(), Some(common::errno::EMSGSIZE));
            } else {
                assert_eq!(res.unwrap(), len);
            }
        }
        let mut seen = Vec::new();
        for _ in 0..7 {
            let got = timeout(Duration::from_secs(5), b.recv())
                .await
                .unwrap()
                .unwrap();
            seen.push(u32::from_be_bytes(got[..].try_into().unwrap()));
        }
        seen.sort_unstable();
        assert_eq!(seen, [0, 1, 2, 4, 5, 6, 7]);
    });
}

#[test]
fn a_send_dropped_before_the_end_of_the_turn_is_not_sent() {
    // The datagram waits in the worker's queue while another task is
    // runnable; dropping the send takes it out of the queue.
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        let busy = giong::spawn(async {
            for _ in 0..4 {
                giong::yield_now().await;
            }
        });
        {
            let mut send = std::pin::pin!(a.send_to(b"dropped".to_vec(), b_addr));
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(std::future::Future::poll(send.as_mut(), cx).is_pending())
            })
            .await;
            assert!(polled, "with another task runnable the send waits");
        }
        busy.await.unwrap();
        a.send_to(b"kept".to_vec(), b_addr).await.0.unwrap();
        let got = timeout(Duration::from_secs(5), b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&got[..], b"kept");
    });
}

#[test]
fn one_task_sends_in_order_while_others_run() {
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        let stop = Rc::new(std::cell::Cell::new(false));
        let spin = {
            let stop = stop.clone();
            giong::spawn(async move {
                while !stop.get() {
                    giong::yield_now().await;
                }
            })
        };
        for i in 0..200u32 {
            a.send_to(i.to_be_bytes().to_vec(), b_addr).await.0.unwrap();
        }
        stop.set(true);
        spin.await.unwrap();
        for i in 0..200u32 {
            let got = timeout(Duration::from_secs(5), b.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(u32::from_be_bytes(got[..].try_into().unwrap()), i);
        }
    });
}

#[test]
fn a_busy_socket_that_goes_quiet_still_wakes_its_receiver() {
    // On epoll a socket that has received many datagrams leaves the epoll
    // set; it must come back when a task waits on it again.
    giong::block_on(async {
        let (a, _) = socket();
        let (b, b_addr) = socket();
        for round in 0..3u32 {
            for i in 0..300u32 {
                a.send_to((round * 1000 + i).to_be_bytes().to_vec(), b_addr)
                    .await
                    .0
                    .unwrap();
                if i % 50 == 49 {
                    // Leave room in the socket's receive buffer.
                    for _ in 0..50 {
                        timeout(Duration::from_secs(5), b.recv())
                            .await
                            .unwrap()
                            .unwrap();
                    }
                }
            }
            // Quiet, then one more datagram from another task while the
            // receiver is already waiting.
            let late = giong::spawn({
                async move {
                    sleep(Duration::from_millis(30)).await;
                    let (c, _) = socket();
                    c.send_to(b"late".to_vec(), b_addr).await.0.unwrap();
                }
            });
            let got = timeout(Duration::from_secs(5), b.recv())
                .await
                .expect("a quiet socket did not wake its receiver")
                .unwrap();
            assert_eq!(&got[..], b"late");
            late.await.unwrap();
        }
    });
}
