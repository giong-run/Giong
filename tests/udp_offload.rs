//! UDP offloads for QUIC (feature `unstable-udp-offload`): GSO sends, GRO
//! receives, ECN bits, the local address of a datagram and the source
//! address of a reply, on io_uring and on epoll.
#![cfg(feature = "unstable-udp-offload")]

use std::io::ErrorKind;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::time::Duration;

use giong::error::Code;
use giong::net::{SendOptions, UdpSocket};
use giong::time::timeout;
use giong::{Builder, DriverKind};

const GRO_BUFFER: usize = giong::net::GRO_RECV_BUFFER;

/// The readiness driver of this system.
#[cfg(target_os = "linux")]
const READINESS: DriverKind = DriverKind::Epoll;
#[cfg(target_os = "macos")]
const READINESS: DriverKind = DriverKind::Kqueue;
/// Windows has one driver, the completion port: both turns run on it.
#[cfg(windows)]
const READINESS: DriverKind = DriverKind::Iocp;

/// The errors a system without an option gives: `ENOPROTOOPT` on macOS;
/// on Windows versions without it `WSAENOPROTOOPT`, or `WSAEINVAL` for an
/// option number they do not know.
#[cfg(target_os = "macos")]
const NO_OPTION: &[i32] = &[libc::ENOPROTOOPT];
#[cfg(windows)]
const NO_OPTION: &[i32] = &[10042, 10022];
#[cfg(target_os = "linux")]
const NO_OPTION: &[i32] = &[];

/// A segmented send without GSO: `EOPNOTSUPP` (Winsock's `WSAEOPNOTSUPP`).
#[cfg(target_os = "macos")]
const NOT_SUPPORTED: i32 = libc::EOPNOTSUPP;
#[cfg(windows)]
const NOT_SUPPORTED: i32 = 10045;

/// Turns GRO on or off; `false` where the system has none (macOS, Windows
/// before 11), which refuses it.
fn gro(s: &UdpSocket, on: bool) -> bool {
    match s.set_gro(on) {
        Ok(()) => true,
        Err(e) if cfg!(not(target_os = "linux")) => {
            let code = e.raw_os_error().unwrap_or(0);
            assert!(NO_OPTION.contains(&code), "{e}");
            false
        }
        Err(e) => panic!("set_gro({on}): {e}"),
    }
}

/// Turns ECN reports on; `false` on a Windows without them (before 11 and
/// Server 2022), which refuses the option. Every other system has them.
fn ecn(s: &UdpSocket) -> bool {
    match s.set_recv_ecn(true) {
        Ok(()) => true,
        Err(e) if cfg!(windows) => {
            let code = e.raw_os_error().unwrap_or(0);
            assert!(NO_OPTION.contains(&code), "{e}");
            false
        }
        Err(e) => panic!("set_recv_ecn: {e}"),
    }
}

/// Addresses of this machine's loopback for a socket bound to every
/// address: all of 127.0.0.0/8 on Linux, 127.0.0.1 alone on macOS.
#[cfg(target_os = "linux")]
const LOOPBACKS: &[u8] = &[1, 2, 77];
#[cfg(target_os = "macos")]
const LOOPBACKS: &[u8] = &[1];
/// Windows receives on all of 127.0.0.0/8, but sends only from 127.0.0.1,
/// the one loopback address its interface has.
#[cfg(windows)]
const LOOPBACKS: &[u8] = &[1];

/// `true` if io_uring works here (it is blocked, for instance, under
/// `scripts/no-io-uring.py`, in Docker with its default seccomp profile, or
/// on a kernel without buffer rings). Where CI says it must work
/// (`GIONG_TEST_EXPECT_DRIVER=io_uring`), not working is a failure.
fn io_uring_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let res = Builder::new()
            .driver(DriverKind::IoUring)
            .block_on(async {});
        if let Err(e) = &res {
            assert_ne!(
                std::env::var("GIONG_TEST_EXPECT_DRIVER").as_deref(),
                Ok("io_uring"),
                "io_uring must work here, but: {e}"
            );
        }
        res.is_ok()
    })
}

/// Runs a test on the `kind` driver; where io_uring cannot be used, its
/// turn goes to the readiness driver, so that every test still runs.
fn run(fut: impl std::future::Future<Output = ()>, kind: DriverKind) {
    let kind = if kind == DriverKind::IoUring
        && (cfg!(not(target_os = "linux")) || !io_uring_available())
    {
        READINESS
    } else {
        kind
    };
    Builder::new()
        .driver(kind)
        .recv_buffers(64, GRO_BUFFER)
        .block_on(fut)
        .unwrap();
}

/// One `send_segments_to` of ten 1200-byte segments arrives as ten
/// datagrams without GRO, and as coalesced ones that cut back into the same
/// ten with GRO.
#[test]
fn a_segmented_send_arrives_as_its_datagrams() {
    for kind in [DriverKind::IoUring, READINESS] {
        for gro in [false, true] {
            run(
                async move {
                    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
                    if !self::gro(&rx, gro) {
                        eprintln!("no GRO here; skipped");
                        return;
                    }
                    let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
                    if !tx.gso_supported() {
                        eprintln!("no UDP_SEGMENT on this kernel; skipped");
                        return;
                    }
                    let payload: Vec<u8> = (0..12_000u32).map(|i| (i / 1200) as u8).collect();
                    let to = rx.local_addr().unwrap();
                    let (res, _) = tx.send_segments_to(payload.clone(), to, 1200, 0).await;
                    assert_eq!(res.unwrap(), 12_000);
                    let mut got: Vec<Vec<u8>> = Vec::new();
                    let mut receives = 0;
                    while got.len() < 10 {
                        let (dg, from) = timeout(Duration::from_secs(5), rx.recv_from())
                            .await
                            .expect("all segments arrive")
                            .unwrap();
                        assert_eq!(from, tx.local_addr().unwrap());
                        assert!(!dg.is_truncated());
                        if !gro {
                            assert_eq!(dg.segment_size(), None);
                        }
                        got.extend(dg.segments().map(<[u8]>::to_vec));
                        receives += 1;
                    }
                    assert_eq!(got.len(), 10);
                    for (i, seg) in got.iter().enumerate() {
                        assert_eq!(seg.len(), 1200);
                        assert!(seg.iter().all(|&b| b == i as u8), "segment {i} in order");
                    }
                    if !gro {
                        assert_eq!(receives, 10);
                    }
                    eprintln!("{kind:?} gro={gro}: 10 segments in {receives} receives");
                },
                kind,
            );
        }
    }
}

/// The last segment may be shorter than the others.
#[test]
fn a_short_last_segment_is_kept() {
    for kind in [DriverKind::IoUring, READINESS] {
        run(
            async {
                let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
                let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
                if !gro(&rx, true) || !tx.gso_supported() {
                    eprintln!("no GRO or GSO here; skipped");
                    return;
                }
                let to = rx.local_addr().unwrap();
                let (res, _) = tx.send_segments_to(vec![7u8; 2500], to, 1000, 0).await;
                assert_eq!(res.unwrap(), 2500);
                let mut lens = Vec::new();
                while lens.iter().sum::<usize>() < 2500 {
                    let (dg, _) = timeout(Duration::from_secs(5), rx.recv_from())
                        .await
                        .unwrap()
                        .unwrap();
                    lens.extend(dg.segments().map(<[u8]>::len));
                }
                assert_eq!(lens, [1000, 1000, 500]);
            },
            kind,
        );
    }
}

/// Without GSO (macOS, Windows before 11), a send of several segments is
/// refused and sends nothing; one of a single segment goes as a datagram.
#[test]
#[cfg(not(target_os = "linux"))]
fn a_segmented_send_is_refused_without_gso() {
    run(
        async {
            let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
            let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
            // macOS has none; a Windows that has it is tested above.
            #[cfg(windows)]
            if tx.gso_supported() {
                eprintln!("this Windows has GSO; skipped");
                return;
            }
            #[cfg(target_os = "macos")]
            assert!(!tx.gso_supported());
            let to = rx.local_addr().unwrap();
            let (res, _) = tx.send_segments_to(vec![7u8; 2500], to, 1000, 0).await;
            let err = res.expect_err("segmented without GSO");
            assert_eq!(err.raw_os_error(), Some(NOT_SUPPORTED), "{err}");
            let (res, _) = tx.send_segments_to(vec![8u8; 900], to, 1000, 0).await;
            assert_eq!(res.unwrap(), 900);
            let (dg, _) = timeout(Duration::from_secs(5), rx.recv_from())
                .await
                .expect("the single segment arrives")
                .unwrap();
            assert_eq!(
                dg.as_slice(),
                &[8u8; 900][..],
                "nothing of the refused send"
            );
        },
        READINESS,
    );
}

/// The ECN bits set on a send are reported on the receive, for IPv4 and
/// IPv6.
#[test]
fn ecn_bits_travel_with_the_datagram() {
    for kind in [DriverKind::IoUring, READINESS] {
        for host in ["127.0.0.1:0", "[::1]:0"] {
            run(
                async move {
                    let Ok(rx) = UdpSocket::bind(host) else {
                        eprintln!("{host} not available; skipped");
                        return;
                    };
                    if !ecn(&rx) {
                        eprintln!("no ECN on this Windows; skipped");
                        return;
                    }
                    let tx = UdpSocket::bind(host).unwrap();
                    let to = rx.local_addr().unwrap();
                    for ecn in [0u8, 1, 2, 3] {
                        let (res, _) = tx.send_segments_to(vec![ecn; 100], to, 0, ecn).await;
                        // Windows lets no sender mark a datagram CE (3), the
                        // mark of a router that met congestion.
                        if cfg!(windows) && ecn == 3 {
                            assert_eq!(res.unwrap_err().kind(), ErrorKind::InvalidInput);
                            continue;
                        }
                        assert_eq!(res.unwrap(), 100);
                        let (dg, _) = timeout(Duration::from_secs(5), rx.recv_from())
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(dg.as_slice(), &[ecn; 100][..]);
                        assert_eq!(dg.ecn(), ecn, "{kind:?} {host}");
                    }
                },
                kind,
            );
        }
    }
}

/// GRO is refused, with its code, on buffers that cannot hold 64 KiB, and
/// receive options are refused once `recv_from` has started.
#[test]
fn offload_options_are_refused_when_they_would_lose_datagrams() {
    Builder::new()
        .block_on(async {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            let e = s.set_gro(true).unwrap_err();
            assert_eq!(
                giong::error::code(&e),
                Some(Code::RecvBufferTooSmall),
                "{e}"
            );
            assert_eq!(e.kind(), ErrorKind::InvalidInput);
            ecn(&s);
        })
        .unwrap();
    run(
        async {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            let _ = timeout(Duration::from_millis(10), s.recv_from()).await;
            let e = s.set_gro(true).unwrap_err();
            assert_eq!(
                giong::error::code(&e),
                Some(Code::RecvAlreadyStarted),
                "{e}"
            );
            let e = s.set_recv_ecn(true).unwrap_err();
            assert_eq!(
                giong::error::code(&e),
                Some(Code::RecvAlreadyStarted),
                "{e}"
            );
        },
        DriverKind::IoUring,
    );
}

/// A socket bound to every address learns which one each datagram was sent
/// to, and answers from that one: the peer sees the address it wrote to.
#[test]
fn a_reply_leaves_from_the_address_the_request_came_to() {
    for kind in [DriverKind::IoUring, READINESS] {
        for gro in [false, true] {
            run(
                async move {
                    let server = UdpSocket::bind("0.0.0.0:0").unwrap();
                    server.set_recv_local_addr(true).unwrap();
                    ecn(&server);
                    if !self::gro(&server, gro) {
                        eprintln!("no GRO here; skipped");
                        return;
                    }
                    let port = server.local_addr().unwrap().port();
                    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
                    for &last in LOOPBACKS {
                        let ip = std::net::IpAddr::from([127, 0, 0, last]);
                        let (res, _) = client.send_to(b"ping".to_vec(), (ip, port).into()).await;
                        res.unwrap();
                        let (dg, from) = timeout(Duration::from_secs(5), server.recv_from())
                            .await
                            .expect("the datagram arrives")
                            .unwrap();
                        assert_eq!(dg.as_slice(), b"ping");
                        assert_eq!(dg.local_ip(), Some(ip), "{kind:?}");
                        let options = SendOptions::new().source(ip).ecn(1);
                        let (res, _) = server.send_to_with(b"pong".to_vec(), from, &options).await;
                        assert_eq!(res.unwrap(), 4);
                        let (dg, from) = timeout(Duration::from_secs(5), client.recv_from())
                            .await
                            .expect("the answer arrives")
                            .unwrap();
                        assert_eq!(dg.as_slice(), b"pong");
                        assert_eq!(from, (ip, port).into(), "answered from {ip}");
                    }
                },
                kind,
            );
        }
    }
}

/// The same over IPv6, and for IPv4 datagrams that reach a dual-stack
/// socket. Skipped where the machine has no IPv6 loopback.
#[test]
fn local_addresses_are_reported_over_ipv6_too() {
    for kind in [DriverKind::IoUring, READINESS] {
        run(
            async move {
                let Ok(server) = UdpSocket::bind("[::]:0") else {
                    eprintln!("no IPv6 here; skipped");
                    return;
                };
                server.set_recv_local_addr(true).unwrap();
                let port = server.local_addr().unwrap().port();
                let v6 = std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST);
                let v4 = std::net::IpAddr::from([127, 0, 0, *LOOPBACKS.last().unwrap()]);
                for (bind, ip) in [("[::1]:0", v6), ("127.0.0.1:0", v4)] {
                    let Ok(client) = UdpSocket::bind(bind) else {
                        eprintln!("cannot bind {bind}; skipped");
                        continue;
                    };
                    let (res, _) = client.send_to(b"ping".to_vec(), (ip, port).into()).await;
                    if res.is_err() {
                        eprintln!("cannot send to {ip}; skipped");
                        continue;
                    }
                    let Ok(got) = timeout(Duration::from_secs(2), server.recv_from()).await else {
                        // An IPv6-only socket (`bindv6only`) gets no IPv4.
                        eprintln!("nothing from {ip}; skipped");
                        continue;
                    };
                    let (dg, from) = got.unwrap();
                    assert_eq!(dg.local_ip(), Some(ip), "{kind:?}");
                    let options = SendOptions::new().source(ip);
                    let (res, _) = server.send_to_with(b"pong".to_vec(), from, &options).await;
                    assert_eq!(res.unwrap(), 4);
                    let (dg, from) = timeout(Duration::from_secs(5), client.recv_from())
                        .await
                        .expect("the answer arrives")
                        .unwrap();
                    assert_eq!(dg.as_slice(), b"pong");
                    assert_eq!(from.port(), port);
                    let seen = match from.ip() {
                        std::net::IpAddr::V6(a) => a.to_ipv4_mapped().map_or(from.ip(), Into::into),
                        a => a,
                    };
                    assert_eq!(seen, ip);
                }
            },
            kind,
        );
    }
}

/// ECN bits sent from a dual-stack socket to an IPv4 peer (a v4-mapped
/// address) arrive. Skipped where the machine has no IPv6.
#[test]
fn ecn_reaches_an_ipv4_peer_of_a_dual_stack_socket() {
    for kind in [DriverKind::IoUring, READINESS] {
        run(
            async move {
                let Ok(sender) = UdpSocket::bind("[::]:0") else {
                    eprintln!("no IPv6 here; skipped");
                    return;
                };
                let receiver = UdpSocket::bind("127.0.0.1:0").unwrap();
                if !ecn(&receiver) {
                    eprintln!("no ECN on this Windows; skipped");
                    return;
                }
                let port = receiver.local_addr().unwrap().port();
                let to = std::net::SocketAddr::from((
                    std::net::Ipv4Addr::LOCALHOST.to_ipv6_mapped(),
                    port,
                ));
                let options = SendOptions::new().ecn(2);
                let (res, _) = sender.send_to_with(b"ect".to_vec(), to, &options).await;
                if res.is_err() {
                    // An IPv6-only socket (`bindv6only`) cannot send IPv4.
                    eprintln!("cannot send to {to}; skipped");
                    return;
                }
                let (dg, _) = timeout(Duration::from_secs(2), receiver.recv_from())
                    .await
                    .expect("the datagram arrives")
                    .unwrap();
                assert_eq!(dg.ecn(), 2, "{kind:?}");
            },
            kind,
        );
    }
}

/// A source address the machine does not have is refused, not replaced.
#[test]
fn a_source_address_the_machine_lacks_is_refused() {
    run(
        async {
            let a = UdpSocket::bind("0.0.0.0:0").unwrap();
            let b = UdpSocket::bind("127.0.0.1:0").unwrap();
            let options = SendOptions::new().source([192, 0, 2, 55].into());
            let (res, _) = a
                .send_to_with(b"x".to_vec(), b.local_addr().unwrap(), &options)
                .await;
            assert!(res.is_err(), "sent from an address that is not ours");
        },
        DriverKind::IoUring,
    );
}

/// macOS: socket buffer sizes are set and read back, and "don't fragment"
/// is `IP_DONTFRAG`.
#[test]
#[cfg(target_os = "macos")]
fn buffer_sizes_and_dont_fragment() {
    run(
        async {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.set_recv_buffer_size(64 * 1024).unwrap();
            s.set_send_buffer_size(48 * 1024).unwrap();
            assert!(s.recv_buffer_size().unwrap() >= 64 * 1024);
            assert!(s.send_buffer_size().unwrap() >= 48 * 1024);
            let flag = |s: &UdpSocket| {
                let mut v: libc::c_int = -1;
                let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: `v` and `len` are valid for the kernel to write.
                let r = unsafe {
                    libc::getsockopt(
                        s.as_raw_fd(),
                        libc::IPPROTO_IP,
                        libc::IP_DONTFRAG,
                        std::ptr::addr_of_mut!(v).cast(),
                        &mut len,
                    )
                };
                assert_eq!(r, 0);
                v != 0
            };
            s.set_dont_fragment(true).unwrap();
            assert!(flag(&s));
            s.set_dont_fragment(false).unwrap();
            assert!(!flag(&s));
        },
        READINESS,
    );
}

/// Socket buffer sizes are set and read back (the kernel doubles them), and
/// "don't fragment" refuses a datagram larger than the interface's MTU
/// instead of fragmenting it.
#[test]
#[cfg(target_os = "linux")]
fn buffer_sizes_and_dont_fragment() {
    run(
        async {
            let s = UdpSocket::bind("127.0.0.1:0").unwrap();
            s.set_recv_buffer_size(64 * 1024).unwrap();
            s.set_send_buffer_size(48 * 1024).unwrap();
            assert!(s.recv_buffer_size().unwrap() >= 64 * 1024);
            assert!(s.send_buffer_size().unwrap() >= 48 * 1024);

            let peer = UdpSocket::bind("127.0.0.1:0").unwrap();
            let to = peer.local_addr().unwrap();
            let mtu: usize = std::fs::read_to_string("/sys/class/net/lo/mtu")
                .ok()
                .and_then(|m| m.trim().parse().ok())
                .unwrap_or(65_536);
            let mode = |s: &UdpSocket| {
                let mut v: libc::c_int = -1;
                let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
                // SAFETY: `v` and `len` are valid for the kernel to write.
                let r = unsafe {
                    libc::getsockopt(
                        s.as_raw_fd(),
                        libc::IPPROTO_IP,
                        libc::IP_MTU_DISCOVER,
                        std::ptr::addr_of_mut!(v).cast(),
                        &mut len,
                    )
                };
                assert_eq!(r, 0);
                v
            };
            s.set_dont_fragment(true).unwrap();
            assert_eq!(mode(&s), libc::IP_PMTUDISC_PROBE);
            s.set_dont_fragment(false).unwrap();
            assert_eq!(mode(&s), libc::IP_PMTUDISC_WANT);
            // Larger than the loopback MTU, within UDP's limit.
            let big = (mtu + 100).min(65_507);
            if big + 28 <= mtu {
                eprintln!("loopback MTU {mtu} is above the largest datagram; the send is skipped");
                return;
            }
            let (res, _) = s.send_to(vec![0u8; big], to).await;
            assert_eq!(res.unwrap(), big, "fragmented without the option");
            s.set_dont_fragment(true).unwrap();
            let (res, _) = s.send_to(vec![0u8; big], to).await;
            let err = res.expect_err("sent a datagram above the MTU with DF set");
            assert_eq!(err.raw_os_error(), Some(libc::EMSGSIZE));
            s.set_dont_fragment(false).unwrap();
            let (res, _) = s.send_to(vec![0u8; big], to).await;
            assert_eq!(res.unwrap(), big);
        },
        DriverKind::IoUring,
    );
}

/// A port the workers of one runtime share: on Windows one socket, whose
/// datagrams a thread of giong's hands out to the workers' sockets. The
/// local address each datagram was sent to comes with it, whichever
/// socket it reaches and whichever way (the socket's own receive, the
/// thread, or the first socket passing on a datagram it took).
#[test]
fn datagrams_of_a_shared_port_keep_their_control_messages() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    const WORKERS: usize = 3;
    // A port below the ephemeral range, free now. On loopback too: a
    // socket of another process bound to 127.0.0.1 alone may not stop the
    // wildcard bind, and would take the datagrams sent here.
    let free = |p: u16| {
        std::net::UdpSocket::bind(("0.0.0.0", p)).is_ok()
            && std::net::UdpSocket::bind(("127.0.0.1", p)).is_ok()
    };
    let port = (20_000u16..30_000)
        .map(|p| p.wrapping_add((std::process::id() % 9_000) as u16))
        .find(|&p| free(p))
        .expect("a free port");
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let bound = Arc::new(AtomicUsize::new(0));
    // How many datagrams the server took and answered: when a reply does
    // not come, they say whether the request or the reply went missing.
    let received = Arc::new(AtomicUsize::new(0));
    let replied = Arc::new(AtomicUsize::new(0));
    let rt = Builder::new()
        .workers(WORKERS)
        .recv_buffers(64, GRO_BUFFER)
        .build()
        .unwrap();
    let handle = rt.handle();
    let b = bound.clone();
    let (rx, tx) = (received.clone(), replied.clone());
    let server = std::thread::spawn(move || {
        rt.run(move |_| {
            let b = b.clone();
            let (rx, tx) = (rx.clone(), tx.clone());
            async move {
                let socket = UdpSocket::bind(addr).unwrap();
                socket.set_recv_local_addr(true).unwrap();
                let index = b.fetch_add(1, Ordering::SeqCst);
                while let Some(Ok((dg, from))) = giong::until_shutdown(socket.recv_from()).await {
                    rx.fetch_add(1, Ordering::SeqCst);
                    let local = dg.local_ip() == Some(std::net::IpAddr::from([127, 0, 0, 1]));
                    let reply = vec![index as u8, u8::from(local)];
                    socket.send_to(reply, from).await.0.unwrap();
                    tx.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
        .unwrap();
    });
    while bound.load(Ordering::SeqCst) < WORKERS {
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut reached = std::collections::HashSet::new();
    let mut sent = 0;
    for i in 0..30 {
        let client = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        for _ in 0..3 {
            client.send_to(b"where", ("127.0.0.1", port)).unwrap();
            sent += 1;
            let mut buf = [0u8; 4];
            let (n, _) = client.recv_from(&mut buf).unwrap_or_else(|e| {
                panic!(
                    "sender {i}: no reply ({e}): {sent} sent, {} received and {} answered by \
                     the server",
                    received.load(Ordering::SeqCst),
                    replied.load(Ordering::SeqCst),
                )
            });
            assert_eq!(n, 2);
            assert_eq!(
                buf[1], 1,
                "sender {i}: no local address on socket {}",
                buf[0]
            );
            reached.insert(buf[0]);
        }
    }
    assert!(reached.len() > 1, "every sender reached one socket");
    handle.shutdown();
    server.join().unwrap();
}
