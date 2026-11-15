//! UDP offloads for QUIC (feature `unstable-udp-offload`): GSO sends, GRO
//! receives, ECN bits, on io_uring and on epoll.
#![cfg(feature = "unstable-udp-offload")]

use std::io::ErrorKind;
use std::time::Duration;

use giong::error::Code;
use giong::net::UdpSocket;
use giong::time::timeout;
use giong::{Builder, DriverKind};

const GRO_BUFFER: usize = 65_627;

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
