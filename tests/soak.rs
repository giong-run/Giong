//! Soak test: hours of churn, then a check that nothing piled up.
//!
//! Every worker runs a TCP echo server and clients that connect to all of
//! them, plus UDP, file and cross-worker traffic. Clients mix clean closes,
//! aborts with unread data, half-closes and receives cancelled by a timeout.
//! A thread outside the runtime samples the process's open descriptors and
//! resident memory once a second.
//!
//! At the end it checks that:
//! * the descriptor count did not grow during the run, and is back to where
//!   it started once the runtime is dropped;
//! * resident memory levelled off after the warm-up;
//! * every worker's driver is back to idle: no tracked operations, nothing in
//!   flight, every receive buffer returned, no stray timers or tasks.
//!
//! It runs for `GIONG_SOAK_SECS` seconds (default 60), so it is not part of
//! the default run:
//!
//! ```text
//! GIONG_SOAK_SECS=3600 cargo test --release --test soak -- --ignored --nocapture
//! ```

use std::cell::Cell;
use std::net::{Shutdown, SocketAddr};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream, UdpSocket};
use giong::time::{sleep, timeout};

mod common;

/// Client tasks per worker.
const CLIENTS: usize = 32;
const RECV_BUFFERS: usize = 512;

#[derive(Default)]
struct Counters {
    connections: AtomicU64,
    round_trips: AtomicU64,
    cancelled_recvs: AtomicU64,
    datagrams: AtomicU64,
    files: AtomicU64,
    cross_worker: AtomicU64,
    /// Errors that none of the scenarios should produce.
    unexpected: AtomicU64,
}

impl Counters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn unexpected(&self, what: &str, err: impl std::fmt::Display) {
        if self.unexpected.fetch_add(1, Ordering::Relaxed) < 20 {
            eprintln!("soak: unexpected error in {what}: {err}");
        }
    }
}

/// xorshift64: each task has its own, seeded from its identity.
struct Rng(Cell<u64>);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(Cell::new(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1))
    }

    fn below(&self, n: u64) -> u64 {
        let mut x = self.0.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0.set(x);
        x % n
    }
}

/// What a client asks the server to do with a message, in its first byte.
const ECHO: u8 = b'e';
/// Wait before echoing, long enough for the client's timeout to fire.
const STALL: u8 = b's';
/// Close the connection without reading the rest.
const HANG_UP: u8 = b'h';

async fn serve(stream: TcpStream, deadline: Instant, counters: Arc<Counters>) {
    loop {
        let left = deadline.saturating_duration_since(Instant::now()) + Duration::from_secs(1);
        let data = match timeout(left, stream.recv()).await {
            Ok(Ok(data)) if data.is_empty() => return,
            Ok(Ok(data)) => data.to_vec(),
            // Clients abort connections on purpose.
            Ok(Err(_)) | Err(_) => return,
        };
        match data[0] {
            HANG_UP => return,
            STALL => sleep(Duration::from_millis(20)).await,
            ECHO => {}
            other => {
                counters.unexpected("server", format!("unknown request {other}"));
                return;
            }
        }
        if stream.write_all(data).await.0.is_err() {
            return;
        }
    }
}

/// Receives exactly `len` bytes.
async fn recv_exact(stream: &TcpStream, len: usize) -> std::io::Result<Vec<u8>> {
    let mut got = Vec::with_capacity(len);
    while got.len() < len {
        let data = stream.recv().await?;
        if data.is_empty() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        got.extend_from_slice(&data);
    }
    Ok(got)
}

/// Waits for `fut`, but not forever: a server that never answers or never
/// closes is a bug the soak test must report, not hang on.
async fn within<T>(
    counters: &Counters,
    what: &str,
    fut: impl std::future::Future<Output = T>,
) -> Option<T> {
    match timeout(Duration::from_secs(10), fut).await {
        Ok(value) => Some(value),
        Err(err) => {
            counters.unexpected(what, err);
            None
        }
    }
}

async fn client(addr: SocketAddr, deadline: Instant, rng: Rng, counters: Arc<Counters>) {
    while Instant::now() < deadline {
        let stream = match TcpStream::connect(addr).await {
            Ok(stream) => stream,
            Err(err) => {
                counters.unexpected("connect", err);
                sleep(Duration::from_millis(10)).await;
                continue;
            }
        };
        Counters::bump(&counters.connections);
        let len = 1 + rng.below(3000) as usize;
        let mut msg = vec![b'x'; len];
        match rng.below(4) {
            // Some round trips, then a clean close.
            0 | 1 => {
                for _ in 0..1 + rng.below(8) {
                    msg[0] = ECHO;
                    let (res, back) = stream.write_all(msg).await;
                    msg = back;
                    if let Err(err) = res {
                        counters.unexpected("write", err);
                        break;
                    }
                    match within(&counters, "echo", recv_exact(&stream, len)).await {
                        Some(Ok(reply)) if reply == msg => Counters::bump(&counters.round_trips),
                        Some(Ok(_)) => counters.unexpected("echo", "reply differs"),
                        Some(Err(err)) => {
                            counters.unexpected("echo", err);
                            break;
                        }
                        None => break,
                    }
                }
            }
            // Ask for a slow reply and give up waiting: a cancelled receive.
            2 => {
                msg[0] = STALL;
                let _ = stream.write_all(msg).await;
                if timeout(Duration::from_millis(1), stream.recv())
                    .await
                    .is_err()
                {
                    Counters::bump(&counters.cancelled_recvs);
                }
                // Dropped with the reply still on its way.
            }
            // Half-close, or have the server hang up on unread data.
            _ => {
                if rng.below(2) == 0 {
                    msg[0] = ECHO;
                    let _ = stream.write_all(msg).await;
                    let _ = stream.shutdown(Shutdown::Write);
                    // Read the echo and then the end of the stream.
                    let _ = within(&counters, "half-close", async {
                        let _ = recv_exact(&stream, len).await;
                        let _ = stream.recv().await;
                    })
                    .await;
                } else {
                    msg[0] = HANG_UP;
                    let _ = stream.write_all(msg).await;
                    let _ = within(&counters, "hang-up", stream.recv()).await;
                }
            }
        }
    }
}

async fn udp(
    peers: Vec<SocketAddr>,
    socket: UdpSocket,
    deadline: Instant,
    counters: Arc<Counters>,
) {
    let rng = Rng::new(socket.local_addr().unwrap().port().into());
    let socket = Rc::new(socket);
    // Answer whatever arrives; stop shortly after the deadline.
    let echo = giong::spawn({
        let (socket, counters) = (socket.clone(), counters.clone());
        async move {
            while let Ok(Ok((dgram, from))) = timeout(
                deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(500),
                socket.recv_from(),
            )
            .await
            {
                if dgram.as_slice().first() == Some(&b'?') {
                    let mut reply = dgram.as_slice().to_vec();
                    drop(dgram);
                    reply[0] = b'!';
                    if let (Err(err), _) = socket.send_to(reply, from).await {
                        counters.unexpected("udp send", err);
                    }
                }
                Counters::bump(&counters.datagrams);
            }
        }
    });
    while Instant::now() < deadline {
        let to = peers[rng.below(peers.len() as u64) as usize];
        let len = 1 + rng.below(1200) as usize;
        let mut msg = vec![b'u'; len];
        msg[0] = b'?';
        if let (Err(err), _) = socket.send_to(msg, to).await {
            counters.unexpected("udp send", err);
        }
        sleep(Duration::from_micros(200)).await;
    }
    let _ = echo.await;
}

async fn files(dir: std::path::PathBuf, deadline: Instant, counters: Arc<Counters>) {
    let rng = Rng::new(giong::worker_id() as u64 + 77);
    let mut n = 0u64;
    while Instant::now() < deadline {
        n += 1;
        let path = dir.join(format!("w{}-{n}", giong::worker_id()));
        let result = async {
            let file = giong::fs::File::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .await?;
            let len = 1 + rng.below(64 * 1024) as usize;
            let (res, _) = file.write_all_at(vec![7u8; len], 0).await;
            res?;
            let (res, buf) = file.read_at(vec![0u8; len], 0).await;
            assert_eq!(res?, len);
            assert!(buf.iter().all(|&b| b == 7));
            assert_eq!(file.metadata().await?.len(), len as u64);
            if rng.below(2) == 0 {
                file.close().await?;
            } // else: closed on drop
            giong::fs::remove_file(&path).await
        }
        .await;
        match result {
            Ok(()) => Counters::bump(&counters.files),
            Err(err) => counters.unexpected("files", err),
        }
    }
}

/// Bounces a message around the workers through `spawn_on` and channels.
async fn cross_worker(workers: usize, deadline: Instant, counters: Arc<Counters>) {
    let handle = giong::handle();
    while Instant::now() < deadline {
        let target = (giong::worker_id() + 1) % workers;
        let (tx, mut rx) = giong::sync::mpsc::channel::<usize>();
        let task = handle.spawn_on(target, move || async move {
            let _ = tx.send(giong::worker_id());
            giong::worker_id()
        });
        match timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(id)) if id == target => {}
            other => counters.unexpected("cross worker", format!("{other:?}")),
        }
        match timeout(Duration::from_secs(5), task).await {
            Ok(Ok(id)) if id == target => Counters::bump(&counters.cross_worker),
            other => counters.unexpected("spawn_on", format!("{other:?}")),
        }
        sleep(Duration::from_micros(100)).await;
    }
}

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

fn rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmRSS:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse().unwrap()
}

fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

#[test]
#[ignore = "runs for GIONG_SOAK_SECS (default 60); run with --ignored"]
fn soak() {
    let secs: u64 = std::env::var("GIONG_SOAK_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
    let fds_before = open_fds();

    // A free port for every worker's listener (SO_REUSEPORT), and one UDP
    // socket per worker, bound up front so that every worker knows the others.
    let (port_holder, port) = common::shared_port();
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let udp_sockets: Vec<_> = (0..workers)
        .map(|_| std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
        .collect();
    let udp_peers: Vec<SocketAddr> = udp_sockets
        .iter()
        .map(|s| s.local_addr().unwrap())
        .collect();
    let udp_sockets = Arc::new(Mutex::new(
        udp_sockets.into_iter().map(Some).collect::<Vec<_>>(),
    ));
    let dir = std::env::temp_dir().join(format!("giong-soak-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let counters = Arc::new(Counters::default());
    let samples = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (samples, stop, counters) = (samples.clone(), stop.clone(), counters.clone());
        std::thread::spawn(move || {
            let start = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                let (fds, rss) = (open_fds() as u64, rss_kib());
                samples.lock().unwrap().push((fds, rss));
                let t = start.elapsed().as_secs();
                if t % 10 == 0 {
                    eprintln!(
                        "soak: {t:>5}s  fds {fds:>4}  rss {:>6} KiB  conns {:>9}  echoes {:>9}  \
                         cancelled {:>8}  datagrams {:>9}  files {:>7}  hops {:>8}",
                        rss,
                        counters.connections.load(Ordering::Relaxed),
                        counters.round_trips.load(Ordering::Relaxed),
                        counters.cancelled_recvs.load(Ordering::Relaxed),
                        counters.datagrams.load(Ordering::Relaxed),
                        counters.files.load(Ordering::Relaxed),
                        counters.cross_worker.load(Ordering::Relaxed),
                    );
                }
            }
        })
    };

    let deadline = Instant::now() + Duration::from_secs(secs);
    let idle = Arc::new(Mutex::new(Vec::new()));
    {
        let (counters, idle, dir) = (counters.clone(), idle.clone(), dir.clone());
        giong::Builder::new()
            .workers(workers)
            .recv_buffers(RECV_BUFFERS, 4096)
            .build()
            .unwrap()
            .run(move |worker| {
                let (counters, idle, dir) = (counters.clone(), idle.clone(), dir.clone());
                let udp_socket = udp_sockets.lock().unwrap()[worker].take().unwrap();
                let udp_peers = udp_peers.clone();
                async move {
                    let listener = TcpListener::bind(addr).expect("bind");
                    let acceptor = giong::spawn({
                        let counters = counters.clone();
                        async move {
                            loop {
                                // Clients on other workers may still be
                                // connecting right after the deadline.
                                let left = deadline.saturating_duration_since(Instant::now())
                                    + Duration::from_secs(2);
                                match timeout(left, listener.accept()).await {
                                    Ok(Ok(stream)) => {
                                        giong::spawn(serve(stream, deadline, counters.clone()));
                                    }
                                    Ok(Err(err)) => counters.unexpected("accept", err),
                                    Err(_) => return,
                                }
                            }
                        }
                    });
                    // Let every worker's listener come up first.
                    sleep(Duration::from_millis(100)).await;

                    let mut tasks = Vec::new();
                    for i in 0..CLIENTS {
                        let rng = Rng::new((worker * CLIENTS + i) as u64 + 1);
                        tasks.push(giong::spawn(client(addr, deadline, rng, counters.clone())));
                    }
                    let udp_socket = UdpSocket::from_std(udp_socket);
                    tasks.push(giong::spawn(udp(
                        udp_peers,
                        udp_socket,
                        deadline,
                        counters.clone(),
                    )));
                    tasks.push(giong::spawn(files(dir, deadline, counters.clone())));
                    tasks.push(giong::spawn(cross_worker(
                        workers,
                        deadline,
                        counters.clone(),
                    )));
                    for task in tasks {
                        task.await.expect("task");
                    }
                    acceptor.await.expect("acceptor");

                    // Server tasks see their clients go away; give the last
                    // closes and cancellations a moment to complete.
                    let settle = Instant::now() + Duration::from_secs(5);
                    loop {
                        sleep(Duration::from_millis(20)).await;
                        let stats = giong::stats();
                        // The main future is not counted as a task; the one
                        // operation left in flight is the eventfd read that
                        // cross-thread wake-ups complete.
                        let quiet = stats.tasks == 0
                            && stats.tracked_ops == 0
                            && stats.inflight <= 1
                            && stats.free_recv_buffers == stats.recv_buffers;
                        if quiet || Instant::now() > settle {
                            idle.lock()
                                .unwrap()
                                .push((worker, format!("{stats:?}"), quiet));
                            break;
                        }
                    }
                }
            })
            .unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    let c = &counters;
    eprintln!(
        "soak: done after {secs}s on {workers} workers: {} connections, {} echoes, \
         {} cancelled receives, {} datagrams, {} files, {} cross-worker hops",
        c.connections.load(Ordering::Relaxed),
        c.round_trips.load(Ordering::Relaxed),
        c.cancelled_recvs.load(Ordering::Relaxed),
        c.datagrams.load(Ordering::Relaxed),
        c.files.load(Ordering::Relaxed),
        c.cross_worker.load(Ordering::Relaxed),
    );

    // 1. Every scenario behaved as designed.
    assert_eq!(c.unexpected.load(Ordering::Relaxed), 0, "unexpected errors");
    assert!(c.connections.load(Ordering::Relaxed) > 0);
    assert!(c.cancelled_recvs.load(Ordering::Relaxed) > 0);

    // 2. Every worker's driver went back to idle.
    for (worker, stats, quiet) in idle.lock().unwrap().iter() {
        assert!(quiet, "worker {worker} did not go idle: {stats}");
    }

    // 3. Descriptors: nothing left once the runtime is gone, and no growth
    //    during the run beyond the connections that happen to be open.
    drop(port_holder);
    assert_eq!(open_fds(), fds_before, "descriptors leaked");
    let samples = samples.lock().unwrap();
    if samples.len() >= 8 {
        let half = samples.len() / 2;
        let first = samples[1..half].iter().map(|s| s.0).max().unwrap();
        let second = samples[half..samples.len() - 1]
            .iter()
            .map(|s| s.0)
            .max()
            .unwrap();
        // Each open connection holds two descriptors, client and server.
        let slack = (2 * workers * CLIENTS + 16) as u64;
        assert!(
            second <= first + slack,
            "descriptor count grew: at most {first} in the first half, {second} in the second"
        );

        // 4. Memory levels off after the first quarter: compare the medians
        //    of the second and the last quarter.
        let quarter = samples.len() / 4;
        let mut q2: Vec<u64> = samples[quarter..2 * quarter].iter().map(|s| s.1).collect();
        let mut q4: Vec<u64> = samples[3 * quarter..].iter().map(|s| s.1).collect();
        let (q2, q4) = (median(&mut q2), median(&mut q4));
        assert!(
            q4 <= q2 + q2 / 10 + 8 * 1024,
            "resident memory kept growing: median {q2} KiB in the second quarter, {q4} KiB in the last"
        );
    }
}
