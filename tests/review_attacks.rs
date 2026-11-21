//! Review harness: hostile and racy use of the drivers. Not for merging.
//!
//! Each test drives one path the way a hostile peer or an impatient caller
//! would, many times, and checks that the runtime neither corrupts data,
//! nor crashes, nor stops serving.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream, UdpSocket};
use giong::time::{sleep, timeout};

fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    (l, addr)
}

/// A cheap deterministic generator, so that runs differ in timing only.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Starts `fut` (one poll, so the operation is handed to the kernel) and
/// drops it at once: a cancellation in flight that does not depend on the
/// timer's resolution (about 15 ms on Windows).
async fn start_and_drop<F: std::future::Future>(fut: F) -> Option<F::Output> {
    let mut fut = std::pin::pin!(fut);
    std::future::poll_fn(|cx| match std::future::Future::poll(fut.as_mut(), cx) {
        std::task::Poll::Ready(v) => std::task::Poll::Ready(Some(v)),
        std::task::Poll::Pending => std::task::Poll::Ready(None),
    })
    .await
}

fn pattern(i: usize) -> u8 {
    (i % 251) as u8
}

/// `recv` is documented cancel safe: dropping it at any moment, many
/// thousands of times, while the peer streams data, must lose and repeat
/// no byte.
#[test]
fn attack_recv_cancel_storm_keeps_the_stream_exact() {
    const TOTAL: usize = 48 << 20;
    let (l, addr) = listener();
    let writer = std::thread::spawn(move || {
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let mut rng = Rng(7);
        let mut sent = 0;
        let mut chunk = vec![0u8; 64 * 1024];
        while sent < TOTAL {
            let n = (rng.next() as usize % chunk.len()).max(1).min(TOTAL - sent);
            for (k, b) in chunk[..n].iter_mut().enumerate() {
                *b = pattern(sent + k);
            }
            c.write_all(&chunk[..n]).unwrap();
            sent += n;
            if rng.next() % 4 == 0 {
                std::thread::sleep(Duration::from_micros(rng.next() % 500));
            }
        }
    });
    giong::block_on(async move {
        let s = l.accept().await.unwrap();
        let mut got = 0usize;
        let mut dropped = 0u64;
        let mut rng = Rng(11);
        let start = Instant::now();
        while got < TOTAL {
            assert!(start.elapsed() < Duration::from_secs(120), "stalled at {got}");
            let wait = Duration::from_micros(rng.next() % 200);
            let r = if rng.next() % 2 == 0 {
                start_and_drop(s.recv()).await.ok_or(())
            } else {
                timeout(wait, s.recv()).await.map_err(|_| ())
            };
            match r {
                Err(_) => dropped += 1,
                Ok(r) => {
                    let data = r.unwrap();
                    assert!(!data.is_empty(), "early end at {got}");
                    for (k, &b) in data.iter().enumerate() {
                        if b != pattern(got + k) {
                            panic!("byte {} is {b}, expected {}", got + k, pattern(got + k));
                        }
                    }
                    got += data.len();
                }
            }
        }
        eprintln!("recv storm: {dropped} receives dropped mid-flight");
        assert!(timeout(Duration::from_secs(5), s.recv()).await.unwrap().unwrap().is_empty());
    });
    writer.join().unwrap();
}

/// Owned-buffer reads dropped while the kernel owns their buffers, and
/// writes dropped while the peer does not read: the driver must keep each
/// buffer alive until the kernel lets go. A mistake here is a use after
/// free, which shows as a crash or as garbage in later reads.
#[test]
fn attack_dropped_reads_and_writes_do_not_corrupt_memory() {
    let (l, addr) = listener();
    let peer = std::thread::spawn(move || {
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_millis(300))).unwrap();
        let mut rng = Rng(3);
        let mut buf = vec![0u8; 64 * 1024];
        let block = vec![0xA5u8; 8 * 1024];
        let check = |d: &[u8]| assert!(d.iter().all(|&b| b == 0x3C), "peer read garbage");
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(8) {
            match rng.next() % 3 {
                0 => {
                    let _ = c.write_all(&block[..(rng.next() as usize % block.len()).max(1)]);
                }
                1 => {
                    if let Ok(n) = c.read(&mut buf) {
                        check(&buf[..n]);
                    }
                }
                _ => std::thread::sleep(Duration::from_micros(rng.next() % 300)),
            }
        }
        // A dropped read may lose bytes, so say "end" until the server goes.
        c.set_read_timeout(Some(Duration::from_millis(10))).unwrap();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if c.write_all(b"!").is_err() {
                break;
            }
            match c.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => check(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => break,
            }
        }
    });
    giong::block_on(async move {
        let s = l.accept().await.unwrap();
        let mut rng = Rng(5);
        let mut reads = 0u64;
        let mut writes = 0u64;
        let mut seen_end = false;
        let start = Instant::now();
        while !seen_end && start.elapsed() < Duration::from_secs(40) {
            let wait = Duration::from_micros(rng.next() % 150);
            let now = rng.next() % 2 == 0;
            if rng.next() % 2 == 0 {
                let r = if now {
                    start_and_drop(s.read(vec![0x5Au8; 32 * 1024])).await.ok_or(())
                } else {
                    timeout(wait, s.read(vec![0x5Au8; 32 * 1024])).await.map_err(|_| ())
                };
                match r {
                    Err(_) => reads += 1,
                    Ok((r, buf)) => {
                        let n = r.unwrap();
                        assert!(n > 0, "peer closed early");
                        for &b in &buf[..n] {
                            assert!(b == 0xA5 || b == b'!', "garbage read: {b:#x}");
                            seen_end |= b == b'!';
                        }
                    }
                }
            } else {
                let len = 1 + rng.next() as usize % (1 << 20);
                let gone = if now {
                    start_and_drop(s.write_all(vec![0x3Cu8; len])).await.is_none()
                } else {
                    timeout(wait, s.write_all(vec![0x3Cu8; len])).await.is_err()
                };
                if gone {
                    writes += 1;
                }
            }
        }
        eprintln!("dropped {reads} reads and {writes} writes in flight");
        assert!(seen_end, "never saw the end");
    });
    peer.join().unwrap();
}

/// Clients that connect and reset at once, thousands of them: the accept
/// loop must keep going and still take the honest client at the end.
#[test]
fn attack_accept_survives_a_reset_storm() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    let (l, addr) = listener();
    let stop = Arc::new(AtomicBool::new(false));
    let storm = Arc::new(AtomicUsize::new(0));
    let stormers: Vec<_> = (0..4)
        .map(|_| {
            let storm = storm.clone();
            std::thread::spawn(move || {
                for _ in 0..1500 {
                    if let Ok(c) = std::net::TcpStream::connect(addr) {
                        let s = socket2::SockRef::from(&c);
                        let _ = s.set_linger(Some(Duration::ZERO));
                        storm.fetch_add(1, Ordering::Relaxed);
                        drop(c);
                    }
                }
            })
        })
        .collect();
    let honest = {
        let storm = storm.clone();
        std::thread::spawn(move || {
            // In the middle of the storm.
            while storm.load(Ordering::Relaxed) < 2000 {
                std::thread::sleep(Duration::from_millis(5));
            }
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            c.write_all(b"honest").unwrap();
            c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let mut b = [0u8; 2];
            c.read_exact(&mut b).unwrap();
            assert_eq!(&b, b"ok");
        })
    };
    let watcher = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            for t in stormers {
                t.join().unwrap();
            }
            honest.join().unwrap();
            stop.store(true, Ordering::Relaxed);
        })
    };
    giong::block_on(async move {
        let start = Instant::now();
        let (mut accepted, mut errors) = (0u64, 0u64);
        while !stop.load(Ordering::Relaxed) {
            assert!(start.elapsed() < Duration::from_secs(120), "storm never ended");
            match timeout(Duration::from_millis(100), l.accept()).await {
                Err(_) => {}
                Ok(Err(e)) => {
                    errors += 1;
                    assert!(errors < 100_000, "accept error loop: {e}");
                }
                Ok(Ok(s)) => {
                    accepted += 1;
                    drop(giong::spawn(async move {
                        if let Ok(Ok(d)) = timeout(Duration::from_secs(10), s.recv()).await {
                            if &*d == b"honest" {
                                s.write_all(&b"ok"[..]).await.0.unwrap();
                            }
                        }
                    }));
                }
            }
        }
        eprintln!("accepted {accepted}, accept errors {errors}");
    });
    watcher.join().unwrap();
    eprintln!("storm connections {}", storm.load(Ordering::Relaxed));
}

/// Connects abandoned in flight (to an address that never answers, and to
/// a closed port), then an ordinary connect.
#[test]
fn attack_abandoned_connects_leave_the_driver_usable() {
    giong::block_on(async {
        let closed = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap()
        };
        for i in 0..2000u32 {
            let target: SocketAddr = if i % 2 == 0 {
                "10.255.255.1:9".parse().unwrap()
            } else {
                closed
            };
            if i % 4 < 2 {
                let _ = start_and_drop(TcpStream::connect(target)).await;
            } else {
                let _ = timeout(Duration::from_micros(50 + (i as u64 % 500)), TcpStream::connect(target)).await;
            }
        }
        let (l, addr) = listener();
        let c = timeout(Duration::from_secs(10), TcpStream::connect(addr)).await.unwrap().unwrap();
        let s = l.accept().await.unwrap();
        c.write_all(&b"x"[..]).await.0.unwrap();
        assert_eq!(&*s.recv().await.unwrap(), b"x");
    });
}

/// Many idle connections, each with a receive waiting: then one byte to
/// each, and each must get it.
#[test]
fn attack_many_idle_connections_all_wake() {
    const N: usize = 1500;
    giong::block_on(async {
        let (l, addr) = listener();
        let mut clients = Vec::new();
        let mut servers = Vec::new();
        for _ in 0..N {
            clients.push(TcpStream::connect(addr).await.unwrap());
            servers.push(l.accept().await.unwrap());
        }
        let handles: Vec<_> = servers
            .into_iter()
            .map(|s| giong::spawn(async move { s.recv().await.map(|d| d.len()) }))
            .collect();
        sleep(Duration::from_millis(200)).await;
        for c in &clients {
            c.write_all(&b"z"[..]).await.0.unwrap();
        }
        for h in handles {
            assert_eq!(timeout(Duration::from_secs(20), h).await.unwrap().unwrap().unwrap(), 1);
        }
    });
}

/// ICMP port unreachable from earlier sends, zero-length datagrams, and
/// datagrams longer than the receive buffer: the receive loop must go on.
#[test]
fn attack_udp_unreachable_empty_and_oversized() {
    giong::block_on(async {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let saddr = server.local_addr().unwrap();
        let dead = {
            let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            s.local_addr().unwrap()
        };
        for _ in 0..200 {
            let _ = server.send_to(&b"to nobody"[..], dead).await;
        }
        let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        peer.send_to(b"", saddr).unwrap();
        peer.send_to(&vec![7u8; 65507], saddr).unwrap();
        peer.send_to(b"after", saddr).unwrap();
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (d, from) = timeout(Duration::from_secs(5), server.recv_from())
                .await
                .expect("receive stuck after ICMP unreachable")
                .expect("receive failed");
            assert_eq!(from, peer.local_addr().unwrap());
            seen.push((d.len(), d.is_truncated()));
        }
        eprintln!("udp: {seen:?}");
        assert_eq!(seen[0], (0, false));
        assert!(seen[1].0 > 0 && (seen[1].0 == 65507 || seen[1].1));
        assert_eq!(seen[2], (5, false));
        // And reads dropped mid-flight on a socket that gets ICMP errors.
        for i in 0..3000u64 {
            if i % 10 == 0 {
                let _ = server.send_to(&b"x"[..], dead).await;
            }
            if i % 2 == 0 {
                let _ = start_and_drop(server.read_from(vec![0u8; 2048])).await;
            } else {
                let _ = timeout(Duration::from_micros(i % 100), server.read_from(vec![0u8; 2048])).await;
            }
        }
        peer.send_to(b"last", saddr).unwrap();
        let mut got_last = false;
        for _ in 0..4 {
            match timeout(Duration::from_secs(5), server.recv_from()).await {
                Ok(Ok((d, _))) if &*d == b"last" => {
                    got_last = true;
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("receive failed: {e}"),
                Err(_) => break,
            }
        }
        assert!(got_last, "datagram after the storm was lost");
    });
}

/// A runtime shut down while every kind of operation is in flight, many
/// times in a row: no hang, no crash.
#[test]
fn attack_shutdown_with_everything_in_flight() {
    for round in 0..40 {
        let rt = giong::Builder::new()
            .workers(4)
            .shutdown_grace(Duration::from_millis(round % 3 * 5))
            .build()
            .unwrap();
        let handle = rt.handle();
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30 + round % 7 * 10));
            handle.shutdown();
        });
        let start = Instant::now();
        rt.run(move |w| async move {
            let (l, addr) = listener();
            let l = Rc::new(l);
            let accept = {
                let l = l.clone();
                giong::spawn(async move {
                    loop {
                        if let Ok(s) = l.accept().await {
                            drop(giong::spawn(async move {
                                loop {
                                    match s.recv().await {
                                        Ok(d) if !d.is_empty() => {
                                            let _ = s.write_all(vec![1u8; 1 << 20]).await;
                                        }
                                        _ => return,
                                    }
                                }
                            }));
                        }
                    }
                })
            };
            for _ in 0..20 {
                drop(giong::spawn(async move {
                    if let Ok(c) = TcpStream::connect(addr).await {
                        loop {
                            let _ = c.write_all(&b"ping"[..]).await;
                            let (r, _) = c.read(vec![0u8; 4096]).await;
                            if r.is_err() {
                                return;
                            }
                        }
                    }
                }));
            }
            let u = UdpSocket::bind("127.0.0.1:0").unwrap();
            drop(giong::spawn(async move {
                let _ = u.recv_from().await;
            }));
            drop(giong::spawn(async move {
                sleep(Duration::from_secs(3600)).await;
            }));
            let _ = w;
            giong::shutdown_signal().await;
            drop(accept);
        })
        .unwrap();
        stopper.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(20), "round {round} took too long");
    }
}

/// File reads and writes abandoned in flight, then a check that the file
/// holds what the completed writes put there.
#[test]
fn attack_file_ops_abandoned_in_flight() {
    let dir = std::env::temp_dir().join(format!("giong-attack-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f");
    std::fs::write(&path, vec![0u8; 8 << 20]).unwrap();
    giong::block_on(async {
        let f = giong::fs::File::options().read(true).write(true).open(&path).await.unwrap();
        let mut rng = Rng(9);
        for i in 0..4000u64 {
            let off = (rng.next() % (7 << 20)) & !4095;
            let wait = Duration::from_micros(rng.next() % 80);
            let now = rng.next() % 2 == 0;
            if i % 2 == 0 {
                if now {
                    let _ = start_and_drop(f.read_at(vec![0u8; 1 << 20], off)).await;
                } else {
                    let _ = timeout(wait, f.read_at(vec![0u8; 1 << 20], off)).await;
                }
            } else if now {
                // Writes of zeros over zeros: a dropped write changes nothing.
                let _ = start_and_drop(f.write_all_at(vec![0u8; 256 * 1024], off)).await;
            } else {
                let _ = timeout(wait, f.write_all_at(vec![0u8; 256 * 1024], off)).await;
            }
        }
        let (r, b) = f.write_all_at(vec![0xEEu8; 4096], 4096).await;
        r.unwrap();
        drop(b);
        let (r, b) = f.read_at(vec![0u8; 3 * 4096], 0).await;
        assert_eq!(r.unwrap(), 3 * 4096);
        assert!(b[..4096].iter().all(|&x| x == 0));
        assert!(b[4096..8192].iter().all(|&x| x == 0xEE));
        assert!(b[8192..].iter().all(|&x| x == 0));
    });
    let _ = std::fs::remove_dir_all(&dir);
}

/// `accept` is documented cancel safe: dropped thousands of times while
/// clients pour in, it must lose no connection.
#[test]
fn attack_accept_cancel_storm_loses_no_connection() {
    const PER: u32 = 600;
    let (l, addr) = listener();
    let clients: Vec<_> = (0..4u32)
        .map(|t| {
            std::thread::spawn(move || {
                let mut fails = 0;
                let mut open = Vec::new();
                for i in 0..PER {
                    let id = (t * PER + i).to_le_bytes();
                    match std::net::TcpStream::connect(addr) {
                        Ok(mut c) => {
                            c.write_all(&id).unwrap();
                            open.push((c, id));
                        }
                        Err(_) => fails += 1,
                    }
                    // Keep a window of connections waiting for their echo.
                    if open.len() >= 32 || i + 1 == PER {
                        for (mut c, id) in open.drain(..) {
                            c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
                            let mut b = [0u8; 4];
                            if c.read_exact(&mut b).is_err() || b != id {
                                fails += 1;
                            }
                        }
                    }
                }
                fails
            })
        })
        .collect();
    giong::block_on(async move {
        let mut rng = Rng(13);
        let mut served = 0;
        let mut dropped = 0u64;
        let start = Instant::now();
        while served < 4 * PER {
            assert!(start.elapsed() < Duration::from_secs(120), "served only {served}");
            let r = if rng.next() % 2 == 0 {
                start_and_drop(l.accept()).await.ok_or(())
            } else {
                timeout(Duration::from_micros(rng.next() % 100), l.accept()).await.map_err(|_| ())
            };
            match r {
                Err(_) => dropped += 1,
                Ok(Err(e)) => panic!("accept failed: {e}"),
                Ok(Ok(s)) => {
                    served += 1;
                    drop(giong::spawn(async move {
                        let mut got = Vec::new();
                        while got.len() < 4 {
                            match s.recv().await {
                                Ok(d) if !d.is_empty() => got.extend_from_slice(&d),
                                _ => return,
                            }
                        }
                        let _ = s.write_all(got).await;
                    }));
                }
            }
        }
        eprintln!("accept storm: {dropped} accepts dropped, {served} served");
        sleep(Duration::from_millis(500)).await;
    });
    let fails: u32 = clients.into_iter().map(|t| t.join().unwrap()).sum();
    assert_eq!(fails, 0, "clients without their echo");
}

/// Datagram sends dropped mid-flight: every datagram that does arrive must
/// be whole, never a buffer freed and reused under the kernel.
#[test]
fn attack_udp_sends_dropped_mid_flight_arrive_whole() {
    let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    peer.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let paddr = peer.local_addr().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        let (mut n, mut bad) = (0u64, 0u64);
        while let Ok(len) = peer.recv(&mut buf) {
            n += 1;
            let first = buf[0];
            if len != 1200 + first as usize * 50 || buf[..len].iter().any(|&b| b != first) {
                bad += 1;
            }
        }
        (n, bad)
    });
    giong::block_on(async move {
        let u = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut rng = Rng(17);
        for _ in 0..40_000u32 {
            let k = (rng.next() % 200) as u8;
            let d = vec![k; 1200 + k as usize * 50];
            if rng.next() % 2 == 0 {
                let _ = start_and_drop(u.send_to(d, paddr)).await;
            } else {
                let _ = timeout(Duration::from_micros(rng.next() % 20), u.send_to(d, paddr)).await;
            }
        }
    });
    let (n, bad) = reader.join().unwrap();
    eprintln!("udp sends: {n} arrived, {bad} damaged");
    assert!(n > 0);
    assert_eq!(bad, 0);
}

/// `send_file` dropped mid-flight, then the file closed at once, many
/// times: the kernel must not read a closed or reused file handle.
#[test]
fn attack_send_file_dropped_and_file_closed() {
    let dir = std::env::temp_dir().join(format!("giong-attack-sf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f");
    std::fs::write(&path, vec![0x77u8; 16 << 20]).unwrap();
    let (l, addr) = listener();
    let peer = std::thread::spawn(move || {
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        let mut buf = vec![0u8; 64 * 1024];
        let (mut total, mut bad) = (0u64, 0u64);
        loop {
            match c.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n as u64;
                    bad += buf[..n].iter().filter(|&&b| b != 0x77).count() as u64;
                    if total % (1 << 20) < 65536 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
        }
        (total, bad)
    });
    giong::block_on(async move {
        let s = l.accept().await.unwrap();
        let mut rng = Rng(19);
        for _ in 0..300 {
            let f = giong::fs::File::open(&path).await.unwrap();
            let len = 1 + rng.next() % (16 << 20);
            if rng.next() % 2 == 0 {
                let _ = start_and_drop(s.send_file(&f, 0, len)).await;
            } else {
                let _ = timeout(Duration::from_micros(rng.next() % 2000), s.send_file(&f, 0, len)).await;
            }
            drop(f);
            // Something else takes the handle number at once.
            let other = std::fs::File::create(dir.join("other")).unwrap();
            drop(other);
        }
        let f = giong::fs::File::open(&path).await.unwrap();
        s.send_file(&f, 0, 1 << 20).await.unwrap();
    });
    let (total, bad) = peer.join().unwrap();
    eprintln!("send_file: {total} bytes arrived, {bad} wrong");
    assert_eq!(bad, 0);
    let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("giong-attack-sf-{}", std::process::id())));
}

/// Diagnosis for the Windows send_file failure: which sequence makes a
/// later `send_file` fail. Prints what each step returns.
#[test]
fn diag_send_file_after_a_dropped_one() {
    let dir = std::env::temp_dir().join(format!("giong-diag-sf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("f");
    std::fs::write(&path, vec![0x77u8; 8 << 20]).unwrap();
    for case in 0..6 {
        let (l, addr) = listener();
        let peer = std::thread::spawn(move || {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            let mut buf = vec![0u8; 64 * 1024];
            let mut total = 0u64;
            // Slow at first, so that a send is still in flight.
            let start = Instant::now();
            loop {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => total += n as u64,
                }
                if start.elapsed() < Duration::from_millis(300) {
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
            total
        });
        let path = path.clone();
        let out = giong::block_on(async move {
            let s = l.accept().await.unwrap();
            let f = giong::fs::File::open(&path).await.unwrap();
            let first = start_and_drop(s.send_file(&f, 0, 8 << 20)).await.is_none();
            let mut steps = vec![format!("first dropped in flight: {first}")];
            match case {
                // A second send_file at once, same file kept open.
                0 => steps.push(format!("send_file at once: {:?}", s.send_file(&f, 0, 1 << 20).await)),
                // A plain write at once.
                1 => steps.push(format!("write_all at once: {:?}", s.write_all(vec![0x77u8; 1 << 20]).await.0)),
                // The file closed, a new one opened, then send_file.
                2 => {
                    drop(f);
                    let g = giong::fs::File::open(&path).await.unwrap();
                    steps.push(format!("new file, send_file at once: {:?}", s.send_file(&g, 0, 1 << 20).await));
                }
                // Wait for the first to end, then send_file.
                3 => {
                    sleep(Duration::from_secs(3)).await;
                    steps.push(format!("after 3 s, send_file: {:?}", s.send_file(&f, 0, 1 << 20).await));
                }
                // The file closed, wait, then send_file on a new one.
                4 => {
                    drop(f);
                    sleep(Duration::from_secs(3)).await;
                    let g = giong::fs::File::open(&path).await.unwrap();
                    steps.push(format!("file closed, after 3 s, send_file: {:?}", s.send_file(&g, 0, 1 << 20).await));
                }
                // Two dropped ones, then send_file.
                _ => {
                    let again = start_and_drop(s.send_file(&f, 0, 8 << 20)).await.is_none();
                    steps.push(format!("second dropped in flight: {again}"));
                    steps.push(format!("third at once: {:?}", s.send_file(&f, 0, 1 << 20).await));
                }
            }
            steps
        });
        let total = peer.join().unwrap();
        eprintln!("diag case {case}: {out:?}, peer got {total} bytes");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
