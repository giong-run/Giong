//! UDP offloads for QUIC (feature `unstable-udp-offload`): GSO sends, GRO
//! receives, ECN bits, the local address of a datagram and the source
//! address of a reply, on io_uring and on epoll.
#![cfg(feature = "unstable-udp-offload")]

use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::time::Duration;

use giong::error::Code;
use giong::net::{SendOptions, UdpSocket};
use giong::time::timeout;
use giong::{Builder, DriverKind};

const GRO_BUFFER: usize = giong::net::GRO_RECV_BUFFER;

fn run(fut: impl std::future::Future<Output = ()>, kind: DriverKind) {
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
    for kind in [DriverKind::IoUring, DriverKind::Epoll] {
        for gro in [false, true] {
            run(
                async move {
                    let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
                    rx.set_gro(gro).unwrap();
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
    for kind in [DriverKind::IoUring, DriverKind::Epoll] {
        run(
            async {
                let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
                rx.set_gro(true).unwrap();
                let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
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

/// The ECN bits set on a send are reported on the receive, for IPv4 and
/// IPv6.
#[test]
fn ecn_bits_travel_with_the_datagram() {
    for kind in [DriverKind::IoUring, DriverKind::Epoll] {
        for host in ["127.0.0.1:0", "[::1]:0"] {
            run(
                async move {
                    let Ok(rx) = UdpSocket::bind(host) else {
                        eprintln!("{host} not available; skipped");
                        return;
                    };
                    rx.set_recv_ecn(true).unwrap();
                    let tx = UdpSocket::bind(host).unwrap();
                    let to = rx.local_addr().unwrap();
                    for ecn in [0u8, 1, 2, 3] {
                        let (res, _) = tx.send_segments_to(vec![ecn; 100], to, 0, ecn).await;
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
            s.set_recv_ecn(true).unwrap();
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
    for kind in [DriverKind::IoUring, DriverKind::Epoll] {
        for gro in [false, true] {
            run(
                async move {
                    let server = UdpSocket::bind("0.0.0.0:0").unwrap();
                    server.set_recv_local_addr(true).unwrap();
                    server.set_recv_ecn(true).unwrap();
                    server.set_gro(gro).unwrap();
                    let port = server.local_addr().unwrap().port();
                    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
                    // All of 127.0.0.0/8 is this machine.
                    for last in [1u8, 2, 77] {
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
    for kind in [DriverKind::IoUring, DriverKind::Epoll] {
        run(
            async move {
                let Ok(server) = UdpSocket::bind("[::]:0") else {
                    eprintln!("no IPv6 here; skipped");
                    return;
                };
                server.set_recv_local_addr(true).unwrap();
                let port = server.local_addr().unwrap().port();
                let v6 = std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST);
                let v4 = std::net::IpAddr::from([127, 0, 0, 3]);
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

/// Socket buffer sizes are set and read back (the kernel doubles them), and
/// "don't fragment" refuses a datagram larger than the interface's MTU
/// instead of fragmenting it.
#[test]
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
