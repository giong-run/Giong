//! Randomized cancellation of reads, writes, flushes and of the whole
//! stream, on whichever driver giong picks; seeded, so a failure repeats. Every byte that arrives is checked against its offset in the
//! stream, so loss, duplication, reordering or corruption shows up; after
//! each round the runtime must hold no operation, buffer or task.

mod common;

use std::time::Duration;

use common::*;
use giong::net::TcpStream;
use giong::time::{sleep, timeout};
use giong_compat::{Compat, Transport};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn dur(&mut self, max_us: u64) -> Duration {
        Duration::from_micros(self.below(max_us + 1))
    }
}

const PERIOD: u64 = 65_521;
const MAX_LEN: usize = 400_000;

fn table() -> &'static [u8] {
    static T: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        (0..PERIOD + MAX_LEN as u64)
            .map(|i| {
                ((i % PERIOD).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8 ^ (i % PERIOD) as u8
            })
            .collect()
    })
}

fn bytes_ref(from: u64, len: usize) -> &'static [u8] {
    assert!(len <= MAX_LEN);
    let at = (from % PERIOD) as usize;
    &table()[at..at + len]
}

fn byte_at(off: u64) -> u8 {
    bytes_ref(off, 1)[0]
}

fn bytes(from: u64, len: usize) -> Vec<u8> {
    bytes_ref(from, len).to_vec()
}

const BUFFERS: [usize; 6] = [1, 7, 512, 4096, 65536, 200_000];

async fn settle() {
    for _ in 0..10 {
        sleep(Duration::from_millis(3)).await;
    }
}

fn assert_idle(what: &str) {
    let st = giong::stats();
    assert_eq!(st.tracked_ops, 0, "{what}: {st:?}");
    assert_eq!(st.free_recv_buffers, st.recv_buffers, "{what}: {st:?}");
    assert_eq!(st.tasks, 0, "{what}: {st:?}");
}

/// Reads everything with giong's own API, pausing at random, checking each
/// byte; returns how many arrived.
async fn checking_drain(s: TcpStream, seed: u64) -> u64 {
    let mut rng = Rng::new(seed ^ 0xABCD);
    let mut got = 0u64;
    loop {
        if rng.below(8) == 0 {
            sleep(rng.dur(2000)).await;
        }
        let Ok(d) = s.recv().await else { return got };
        if d.is_empty() {
            return got;
        }
        for (i, b) in d.iter().enumerate() {
            assert_eq!(
                *b,
                byte_at(got + i as u64),
                "corrupt byte at {}",
                got + i as u64
            );
        }
        got += d.len() as u64;
    }
}

/// Writes with random sizes and random cancellations of writes, flushes and
/// reads; at the end flushes and shuts down (`graceful`) or drops the stream.
#[test]
fn random_write_side_cancellation() {
    giong::block_on(async {
        for seed in 0..60u64 {
            let mut rng = Rng::new(seed);
            let (c, s) = pair().await;
            let server = giong::spawn(checking_drain(s, seed));
            let limit = BUFFERS[rng.below(BUFFERS.len() as u64) as usize];
            let mut c = Compat::new(c).write_buffer(limit);
            let mut accepted = 0u64;
            for _ in 0..150 {
                match rng.below(20) {
                    0..=9 => {
                        let len = rng.below(300_000) as usize + 1;
                        let buf = bytes_ref(accepted, len);
                        match timeout(rng.dur(3000), c.write(buf)).await {
                            Ok(Ok(n)) => {
                                assert!(n > 0 && n <= len);
                                accepted += n as u64;
                            }
                            Ok(Err(e)) => panic!("write: {e}"),
                            Err(_) => {}
                        }
                    }
                    10..=13 => {
                        if let Ok(r) = timeout(rng.dur(3000), c.flush()).await {
                            r.unwrap();
                        }
                    }
                    14..=16 => {
                        let mut b = [0u8; 64];
                        assert!(timeout(rng.dur(2000), c.read(&mut b)).await.is_err());
                    }
                    _ => sleep(rng.dur(1000)).await,
                }
            }
            if seed % 2 == 0 {
                timeout(LIMIT, c.flush()).await.unwrap().unwrap();
                timeout(LIMIT, c.shutdown()).await.unwrap().unwrap();
                let got = timeout(LIMIT, server).await.unwrap().unwrap();
                assert_eq!(got, accepted, "seed {seed}: graceful end lost data");
                drop(c);
            } else {
                drop(c);
                let got = timeout(LIMIT, server).await.unwrap().unwrap();
                assert!(got <= accepted, "seed {seed}: {got} > {accepted}");
            }
            settle().await;
            assert_idle(&format!("write side, seed {seed}"));
        }
    });
}

/// The peer sends `total` bytes at random pace; the reader mixes `read`
/// with random buffer sizes and `fill_buf` with partial `consume`, and
/// drops reads at random.
#[test]
fn random_read_side_cancellation() {
    giong::block_on(async {
        for seed in 0..60u64 {
            let mut rng = Rng::new(seed);
            let (c, s) = pair().await;
            let total = rng.below(4 << 20) + 1;
            let server = giong::spawn(async move {
                let mut rng = Rng::new(seed ^ 0x5555);
                let mut off = 0u64;
                while off < total {
                    let len = (rng.below(100_000) + 1).min(total - off) as usize;
                    s.write_all(bytes(off, len)).await.0.unwrap();
                    off += len as u64;
                    if rng.below(4) == 0 {
                        sleep(rng.dur(1500)).await;
                    }
                }
                s.shutdown(std::net::Shutdown::Write).unwrap();
                s
            });
            let mut c = Compat::new(c);
            let mut got = 0u64;
            loop {
                let wait = rng.dur(2000);
                let n = if rng.below(2) == 0 {
                    let mut buf = vec![0u8; rng.below(20_000) as usize + 1];
                    match timeout(wait, c.read(&mut buf)).await {
                        Ok(r) => {
                            let n = r.unwrap();
                            assert_eq!(buf[..n], *bytes_ref(got, n), "seed {seed} at {got}");
                            n
                        }
                        Err(_) => continue,
                    }
                } else {
                    let want = rng.below(20_000) as usize + 1;
                    let res = timeout(wait, async {
                        let avail = c.fill_buf().await.unwrap();
                        let n = avail.len().min(want);
                        assert_eq!(avail[..n], *bytes_ref(got, n), "seed {seed} at {got}");
                        n
                    })
                    .await;
                    match res {
                        Ok(n) => {
                            c.consume(n);
                            n
                        }
                        Err(_) => continue,
                    }
                };
                if n == 0 {
                    break;
                }
                got += n as u64;
                if rng.below(10) == 0 {
                    sleep(rng.dur(1000)).await;
                }
            }
            assert_eq!(got, total, "seed {seed}");
            drop(timeout(LIMIT, server).await.unwrap().unwrap());
            drop(c);
            settle().await;
            assert_idle(&format!("read side, seed {seed}"));
        }
    });
}

/// Both directions at once over split halves in two tasks, with a native
/// echo on the other side, random cancellations on both halves.
async fn split_echo_round<S: Transport>(c: Compat<S>, seed: u64, total: u64) {
    let mut rng = Rng::new(seed ^ 0x77);
    let (mut rd, mut wr) = tokio::io::split(c);
    let writer = giong::spawn(async move {
        let mut rng = Rng::new(seed ^ 0x99);
        let mut accepted = 0u64;
        while accepted < total {
            match rng.below(10) {
                0..=6 => {
                    let len = (rng.below(150_000) + 1).min(total - accepted) as usize;
                    let buf = bytes_ref(accepted, len);
                    if let Ok(r) = timeout(rng.dur(3000), wr.write(buf)).await {
                        accepted += r.unwrap() as u64;
                    }
                }
                7..=8 => {
                    if let Ok(r) = timeout(rng.dur(3000), wr.flush()).await {
                        r.unwrap();
                    }
                }
                _ => sleep(rng.dur(800)).await,
            }
        }
        timeout(LIMIT, wr.shutdown()).await.unwrap().unwrap();
        wr
    });
    let mut got = 0u64;
    loop {
        let mut buf = vec![0u8; rng.below(30_000) as usize + 1];
        let n = match timeout(rng.dur(2500), rd.read(&mut buf)).await {
            Ok(r) => r.unwrap(),
            Err(_) => continue,
        };
        if n == 0 {
            break;
        }
        assert_eq!(buf[..n], *bytes_ref(got, n), "seed {seed} at {got}");
        got += n as u64;
    }
    assert_eq!(got, total, "seed {seed}");
    let wr = timeout(LIMIT, writer).await.unwrap().unwrap();
    drop(rd.unsplit(wr));
}

async fn native_echo(s: TcpStream) {
    loop {
        let Ok(d) = s.recv().await else { return };
        if d.is_empty() {
            let _ = s.shutdown(std::net::Shutdown::Write);
            return;
        }
        let v = d.to_vec();
        drop(d);
        if s.write_all(v).await.0.is_err() {
            return;
        }
    }
}

#[test]
fn random_split_echo_tcp() {
    giong::block_on(async {
        for seed in 0..40u64 {
            let mut rng = Rng::new(seed);
            let (c, s) = pair().await;
            let server = giong::spawn(native_echo(s));
            let limit = BUFFERS[rng.below(BUFFERS.len() as u64) as usize];
            let total = rng.below(if limit < 4096 { 40_000 } else { 3 << 20 }) + 1;
            split_echo_round(Compat::new(c).write_buffer(limit), seed, total).await;
            timeout(LIMIT, server).await.unwrap().unwrap();
            settle().await;
            assert_idle(&format!("split echo, seed {seed}"));
        }
    });
}

#[test]
fn random_split_echo_tls() {
    use giong_tls::{TlsAcceptor, TlsConnector};
    giong::block_on(async {
        let pki = pki();
        for seed in 0..25u64 {
            let mut rng = Rng::new(seed);
            let (c, s) = pair().await;
            let acceptor = TlsAcceptor::new(server_config(&pki, &[]));
            let server = giong::spawn(async move {
                let tls = acceptor.accept(s).await.unwrap();
                loop {
                    let d = tls.recv().await.unwrap();
                    if d.is_empty() {
                        tls.shutdown().await.unwrap();
                        return;
                    }
                    tls.write_all(d).await.0.unwrap();
                }
            });
            let connector = TlsConnector::new(client_config(&pki, &[]));
            let tls = connector.connect("localhost", c).await.unwrap();
            let limit = BUFFERS[rng.below(BUFFERS.len() as u64) as usize];
            let total = rng.below(if limit < 4096 { 40_000 } else { 2 << 20 }) + 1;
            split_echo_round(Compat::new(tls).write_buffer(limit), seed, total).await;
            timeout(LIMIT, server).await.unwrap().unwrap();
            settle().await;
            assert_idle(&format!("tls split echo, seed {seed}"));
        }
    });
}

/// TLS 1.3 key updates in the middle of a bidirectional transfer: the
/// server (tokio-rustls over `Compat`) refreshes its keys every few reads,
/// so the client's giong-tls must answer with its own key update while the
/// client's writer task is sending through the same stream.
#[test]
fn split_echo_tls_with_key_updates() {
    use giong_tls::TlsConnector;
    giong::block_on(async {
        let pki = pki();
        for seed in 0..15u64 {
            let mut rng = Rng::new(seed);
            let (c, s) = pair().await;
            let acceptor = tokio_rustls::TlsAcceptor::from(server_config(&pki, &[]));
            let server = giong::spawn(async move {
                let mut tls = acceptor.accept(Compat::new(s)).await.unwrap();
                let mut buf = vec![0u8; 16384];
                let mut reads = 0u64;
                let mut updates = 0u64;
                loop {
                    let n = tls.read(&mut buf).await.unwrap();
                    if n == 0 {
                        tls.shutdown().await.unwrap();
                        return updates;
                    }
                    tls.write_all(&buf[..n]).await.unwrap();
                    reads += 1;
                    if reads % 7 == 0 {
                        tls.get_mut().1.refresh_traffic_keys().unwrap();
                        updates += 1;
                    }
                    tls.flush().await.unwrap();
                }
            });
            let connector = TlsConnector::new(client_config(&pki, &[]));
            let tls = connector.connect("localhost", c).await.unwrap();
            let limit = BUFFERS[2 + rng.below(4) as usize];
            let total = rng.below(2 << 20) + 100_000;
            split_echo_round(Compat::new(tls).write_buffer(limit), seed, total).await;
            let updates = timeout(LIMIT, server).await.unwrap().unwrap();
            assert!(updates > 0);
            settle().await;
            assert_idle(&format!("tls key update, seed {seed}"));
        }
    });
}
