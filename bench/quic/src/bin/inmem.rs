//! quinn-proto with no I/O at all: a client and the benchmark's server
//! engine in one thread, datagrams handed over in memory. What the server
//! costs here is the floor any I/O layer adds to; the rest of
//! `srv_ns_per_kib` in bench/quic.sh is the I/O layer.
//!
//!   inmem --size 16777216 --secs 3 --segments 10
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::BytesMut;
use quinn_proto::{DatagramEvent, Dir, Endpoint, EndpointConfig, Event, StreamEvent, StreamId};

include!("../args.rs");

fn thread_cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid timespec for the kernel to fill.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn main() {
    let size: u64 = arg("size", 16 << 20);
    let secs: f64 = arg("secs", 3.0);
    let segments: usize = arg("segments", 10);
    let saddr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
    let caddr: SocketAddr = "127.0.0.1:5555".parse().unwrap();

    let mut server = bench_quic::engine::Engine::new(segments);
    let mut cep = Endpoint::new(Arc::new(EndpointConfig::default()), None, true, None);
    let now = Instant::now();
    let (ch, mut conn) = cep
        .connect(now, bench_quic::client_config(), saddr, "localhost")
        .unwrap();
    let mut c2s: VecDeque<(Vec<u8>, Option<usize>)> = VecDeque::new();
    let mut s2c: VecDeque<(Vec<u8>, Option<usize>)> = VecDeque::new();
    let mut buf = Vec::with_capacity(65536);
    let mut stream: Option<StreamId> = None;
    let (mut got, mut total, mut done) = (0u64, 0u64, 0u64);
    let (mut srv_cpu, mut start) = (Duration::ZERO, None::<(Instant, Duration, u64)>);
    let mut connected = false;
    loop {
        let now = Instant::now();
        // Client side.
        if conn.poll_timeout().is_some_and(|t| t <= now) {
            conn.handle_timeout(now);
        }
        while let Some((d, seg)) = s2c.pop_front() {
            let seg = seg.unwrap_or(d.len());
            for one in d.chunks(seg) {
                buf.clear();
                if let Some(DatagramEvent::ConnectionEvent(_, ev)) =
                    cep.handle(now, saddr, None, None, BytesMut::from(one), &mut buf)
                {
                    conn.handle_event(ev);
                }
            }
        }
        while let Some(ev) = conn.poll_endpoint_events() {
            if let Some(cev) = cep.handle_event(ch, ev) {
                conn.handle_event(cev);
            }
        }
        while let Some(ev) = conn.poll() {
            match ev {
                Event::Connected => connected = true,
                Event::Stream(StreamEvent::Readable { id }) if Some(id) == stream => {
                    let mut rs = conn.recv_stream(id);
                    let mut chunks = rs.read(true).unwrap();
                    loop {
                        match chunks.next(usize::MAX) {
                            Ok(Some(c)) => {
                                got += c.bytes.len() as u64;
                                total += c.bytes.len() as u64;
                            }
                            Ok(None) => {
                                stream = None;
                                done += 1;
                                break;
                            }
                            Err(_) => break,
                        }
                    }
                    let _ = chunks.finalize();
                }
                Event::ConnectionLost { reason } => panic!("lost: {reason}"),
                _ => {}
            }
        }
        if connected && stream.is_none() {
            let id = conn.streams().open(Dir::Bi).unwrap();
            conn.send_stream(id).write(&size.to_be_bytes()).unwrap();
            conn.send_stream(id).finish().unwrap();
            stream = Some(id);
            got = 0;
        }
        loop {
            buf.clear();
            match conn.poll_transmit(now, segments, &mut buf) {
                Some(t) => c2s.push_back((buf[..t.size].to_vec(), t.segment_size)),
                None => break,
            }
        }
        // Server side, timed.
        let t0 = thread_cpu();
        let mut out =
            |t: &quinn_proto::Transmit, d: &[u8]| s2c.push_back((d.to_vec(), t.segment_size));
        while let Some((d, _)) = c2s.pop_front() {
            server.handle(
                now,
                caddr,
                None,
                None,
                BytesMut::from(&d[..]),
                None,
                &mut out,
            );
        }
        server.drive(now, &mut out);
        srv_cpu += thread_cpu() - t0;

        if start.is_none() && total > 4 * size.min(16 << 20) {
            start = Some((Instant::now(), srv_cpu, total));
        }
        if let Some((t, c0, b0)) = start {
            if t.elapsed().as_secs_f64() >= secs {
                let el = t.elapsed().as_secs_f64();
                let b = (total - b0) as f64;
                let cpu = (srv_cpu - c0).as_secs_f64();
                println!(
                    "inmem segments={segments} size={size} gbit_s={:.3} srv_ns_per_kib={:.1} srv_share_pct={:.1} streams={done} last_stream_bytes={got}",
                    b * 8.0 / el / 1e9,
                    cpu * 1e9 / (b / 1024.0),
                    cpu / el * 100.0
                );
                return;
            }
        }
    }
}
