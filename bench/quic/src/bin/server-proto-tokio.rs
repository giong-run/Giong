//! The shared quinn-proto engine on a current-thread Tokio runtime, with
//! quinn-udp for the socket (GSO and GRO, as quinn itself does). Isolates
//! quinn's own architecture (task per connection, locks, channels) from the
//! I/O layer.
use std::collections::VecDeque;
use std::io::IoSliceMut;
use std::net::SocketAddr;
use std::time::Instant;

use bytes::BytesMut;
use quinn_udp::{RecvMeta, Transmit as UdpTransmit, UdpSockRef, UdpSocketState, BATCH_SIZE};
use tokio::io::Interest;

include!("../args.rs");

struct Pending {
    dest: SocketAddr,
    seg: Option<usize>,
    data: Vec<u8>,
}

fn main() {
    let addr: SocketAddr = arg("addr", "127.0.0.1:4433".parse().unwrap());
    let gso: bool = arg("gso", 1u8) == 1;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let std_sock = std::net::UdpSocket::bind(addr).unwrap();
        let state = UdpSocketState::new(UdpSockRef::from(&std_sock)).unwrap();
        std_sock.set_nonblocking(true).unwrap();
        let sock = tokio::net::UdpSocket::from_std(std_sock).unwrap();
        let segs = if gso { state.max_gso_segments().min(10) } else { 1 };
        let gro = if gso { state.gro_segments() } else { 1 };
        eprintln!("proto-tokio listening on {addr} (gso segments {segs}, gro {gro})");
        let mut engine = bench_quic::engine::Engine::new(segs);
        let chunk = 1500 * gro;
        let mut rbuf = vec![0u8; chunk * BATCH_SIZE];
        let mut meta = [RecvMeta::default(); BATCH_SIZE];
        let mut pending: VecDeque<Pending> = VecDeque::new();
        loop {
            let now = Instant::now();
            let deadline = {
                let mut out = |t: &quinn_proto::Transmit, d: &[u8]| {
                    if pending.is_empty() {
                        let ut = UdpTransmit {
                            destination: t.destination,
                            ecn: t.ecn.and_then(|e| quinn_udp::EcnCodepoint::from_bits(e as u8)),
                            contents: d,
                            segment_size: t.segment_size,
                            src_ip: t.src_ip,
                        };
                        match sock.try_io(Interest::WRITABLE, || state.send(UdpSockRef::from(&sock), &ut)) {
                            Ok(()) => return,
                            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                            Err(_) => return,
                        }
                    }
                    pending.push_back(Pending { dest: t.destination, seg: t.segment_size, data: d.to_vec() });
                };
                engine.drive(now, &mut out)
            };
            let sleep = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                r = sock.writable(), if !pending.is_empty() => {
                    if r.is_ok() {
                        while let Some(p) = pending.front() {
                            let ut = UdpTransmit { destination: p.dest, ecn: None, contents: &p.data, segment_size: p.seg, src_ip: None };
                            match sock.try_io(Interest::WRITABLE, || state.send(UdpSockRef::from(&sock), &ut)) {
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                                _ => { pending.pop_front(); }
                            }
                        }
                    }
                }
                r = sock.readable() => {
                    if r.is_err() { continue; }
                    let now = Instant::now();
                    loop {
                        let mut bufs: Vec<IoSliceMut<'_>> = rbuf.chunks_mut(chunk).map(IoSliceMut::new).collect();
                        let n = match sock.try_io(Interest::READABLE, || state.recv(UdpSockRef::from(&sock), &mut bufs, &mut meta)) {
                            Ok(n) => n,
                            Err(_) => break,
                        };
                        drop(bufs);
                        for (i, m) in meta.iter().take(n).enumerate() {
                            let data = BytesMut::from(&rbuf[i * chunk..i * chunk + m.len]);
                            let seg = if m.stride < m.len { Some(m.stride) } else { None };
                            let mut out = |t: &quinn_proto::Transmit, d: &[u8]| {
                                pending.push_back(Pending { dest: t.destination, seg: t.segment_size, data: d.to_vec() });
                            };
                            let ecn = m.ecn.and_then(|e| quinn_proto::EcnCodepoint::from_bits(e as u8));
                            engine.handle(now, m.addr, m.dst_ip, ecn, data, seg, &mut out);
                        }
                    }
                }
                _ = sleep => {}
            }
        }
    });
}
