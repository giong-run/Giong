//! The shared quinn-proto engine on one giong worker.
//!
//! `--offload 0`: the released `UdpSocket` API only: multishot `recv_from`
//! from the worker's buffer pool, one `send_to` per datagram.
//! `--offload 1` (default): the `unstable-udp-offload` prototype: GRO
//! receives into 64 KiB pool buffers, ECN, and one `send_segments_to` (GSO)
//! per quinn-proto transmit of up to 10 datagrams, as quinn does.
use std::cell::RefCell;
use std::future::{poll_fn, Future};
use std::net::SocketAddr;
use std::pin::pin;
use std::rc::Rc;
use std::task::Poll;
use std::time::Instant;

use bytes::BytesMut;
use giong::net::{Datagram, UdpSocket};

include!("../args.rs");

/// The next datagram if one is already there, without waiting.
async fn try_recv(sock: &UdpSocket) -> Option<std::io::Result<(Datagram, SocketAddr)>> {
    let mut f = pin!(sock.recv_from());
    poll_fn(|cx| match f.as_mut().poll(cx) {
        Poll::Ready(r) => Poll::Ready(Some(r)),
        Poll::Pending => Poll::Ready(None),
    })
    .await
}

fn main() {
    let addr: SocketAddr = arg("addr", "127.0.0.1:4433".parse().unwrap());
    let offload: bool = arg("offload", 1u8) == 1;
    let epoll: bool = arg("epoll", 0u8) == 1;
    // quinn sends at most 10 datagrams per GSO send; Linux takes up to 64
    // and 64 KiB.
    let segments: usize = arg("segments", 10);
    // One engine and socket per worker, all bound to the same port: the
    // kernel spreads clients over them by address.
    let workers: usize = arg("workers", 1);
    let mut b = giong::Builder::new().workers(workers);
    if offload {
        b = b.recv_buffers(256, 65_627);
    }
    if epoll {
        b = b.driver(giong::DriverKind::Epoll);
    }
    b.build()
        .unwrap()
        .run(move |worker| serve(worker, addr, offload, segments))
        .unwrap();
}

async fn serve(worker: usize, addr: SocketAddr, offload: bool, segments: usize) {
    let sock = Rc::new(UdpSocket::bind(addr).unwrap());
    if offload {
        sock.set_gro(true).unwrap();
        sock.set_recv_ecn(true).unwrap();
    }
    if worker == 0 {
        eprintln!(
            "proto-giong listening on {addr} offload={offload} segments={segments} ({:?})",
            giong::driver_choice()
        );
    }
    let mut engine = bench_quic::engine::Engine::new(if offload { segments } else { 1 });
    let pool: Rc<RefCell<Vec<Vec<u8>>>> = Rc::new(RefCell::new(Vec::new()));
    let mut out = |t: &quinn_proto::Transmit, d: &[u8]| {
        let mut v = pool
            .borrow_mut()
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(1500));
        v.extend_from_slice(d);
        let (sock, pool, dest) = (sock.clone(), pool.clone(), t.destination);
        let seg = t.segment_size.unwrap_or(0);
        let ecn = t.ecn.map_or(0, |e| e as u8);
        giong::spawn(async move {
            let (_, mut v) = if offload {
                sock.send_segments_to(v, dest, seg, ecn).await
            } else {
                sock.send_to(v, dest).await
            };
            v.clear();
            pool.borrow_mut().push(v);
        });
    };
    loop {
        let now = Instant::now();
        let deadline = engine.drive(now, &mut out);
        let first = match deadline {
            Some(d) => giong::time::timeout(d.saturating_duration_since(now), sock.recv_from())
                .await
                .ok(),
            None => Some(sock.recv_from().await),
        };
        let Some(first) = first else { continue };
        let now = Instant::now();
        let mut next = Some(first);
        let mut n = 0;
        while let Some(r) = next {
            if let Ok((dg, from)) = r {
                let data = BytesMut::from(dg.as_slice());
                let (seg, ecn) = if offload {
                    (
                        dg.segment_size(),
                        quinn_proto::EcnCodepoint::from_bits(dg.ecn()),
                    )
                } else {
                    (None, None)
                };
                drop(dg);
                engine.handle(now, from, None, ecn, data, seg, &mut out);
            }
            n += 1;
            next = if n < 64 { try_recv(&sock).await } else { None };
        }
    }
}
