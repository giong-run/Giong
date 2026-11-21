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
            match timeout(wait, s.recv()).await {
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
            if rng.next() % 2 == 0 {
                match timeout(wait, s.read(vec![0x5Au8; 32 * 1024])).await {
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
                if timeout(wait, s.write_all(vec![0x3Cu8; len])).await.is_err() {
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
    let (l, addr) = listener();
    let stormers: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                let mut n = 0;
                for _ in 0..1500 {
                    if let Ok(c) = std::net::TcpStream::connect(addr) {
                        let s = socket2::SockRef::from(&c);
                        let _ = s.set_linger(Some(Duration::ZERO));
                        n += 1;
                        drop(c);
                    }
                }
                n
            })
        })
        .collect();
    let honest = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.write_all(b"honest").unwrap();
        c.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut b = [0u8; 2];
        c.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ok");
    });
    giong::block_on(async move {
        let start = Instant::now();
        let mut accepted = 0u64;
        let mut errors = 0u64;
        loop {
            assert!(start.elapsed() < Duration::from_secs(60), "honest client never served");
            match timeout(Duration::from_secs(30), l.accept()).await.expect("accept hung") {
                Err(e) => {
                    errors += 1;
                    assert!(errors < 100_000, "accept error loop: {e}");
                }
                Ok(s) => {
                    accepted += 1;
                    // Peek without blocking the loop for long.
                    if let Ok(Ok(d)) = timeout(Duration::from_millis(1), s.recv()).await {
                        if &*d == b"honest" {
                            s.write_all(&b"ok"[..]).await.0.unwrap();
                            break;
                        }
                    }
                }
            }
        }
        eprintln!("accepted {accepted}, accept errors {errors}");
    });
    honest.join().unwrap();
    let total: usize = stormers.into_iter().map(|t| t.join().unwrap()).sum();
    eprintln!("storm connections {total}");
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
            let _ = timeout(Duration::from_micros(50 + (i as u64 % 500)), TcpStream::connect(target)).await;
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
            let _ = timeout(Duration::from_micros(i % 100), server.read_from(vec![0u8; 2048])).await;
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
            if i % 2 == 0 {
                let _ = timeout(wait, f.read_at(vec![0u8; 1 << 20], off)).await;
            } else {
                // Writes of zeros over zeros: a dropped write changes nothing.
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
