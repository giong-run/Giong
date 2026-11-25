//! Reactor latency while CPU-heavy jobs run next to the network workers.
//!
//! An echo server answers small TCP pings from client threads (one ping per
//! connection every millisecond) while CPU-bound jobs keep every core busy.
//! The question is how much the CPU work delays the network work, and how
//! many jobs get done.
//!
//!     bench-lanes <scenario> <seconds> <port>
//!
//! Scenarios:
//!   idle          giong, no CPU jobs: the floor
//!   inline        giong, jobs run inside worker tasks (what the lanes avoid)
//!   lane-nice0    giong, jobs on the compute lane with the lower priority
//!                 undone (needs root or CAP_SYS_NICE): the lane alone
//!   lane          giong, jobs on the compute lane as shipped (nice +10)
//!   tokio-idle    Tokio multi-thread runtime, no CPU jobs
//!   tokio         Tokio, jobs through spawn_blocking
//!
//! Every scenario keeps one job in flight per CPU, so the CPU work offered is
//! the same. Prints one CSV line:
//!   scenario,pings,p50_us,p99_us,p999_us,max_us,jobs_per_s

use std::hint::black_box;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CLIENTS: usize = 8;
const PING: usize = 64;
/// About 2 ms of CPU on a current core.
const JOB_ITERS: u64 = 750_000;

fn job() -> u64 {
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in 0..JOB_ITERS {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x = x.wrapping_add(black_box(i));
    }
    x
}

fn cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let scenario = args.get(1).map(String::as_str).unwrap_or("lane").to_owned();
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let port: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(9300);
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let stop = Arc::new(AtomicBool::new(false));
    let jobs = Arc::new(AtomicU64::new(0));

    let server = {
        let (scenario, stop, jobs) = (scenario.clone(), stop.clone(), jobs.clone());
        std::thread::spawn(move || match scenario.as_str() {
            "tokio" | "tokio-idle" => tokio_server(addr, scenario == "tokio", stop, jobs),
            _ => giong_server(addr, scenario, stop, jobs),
        })
    };
    // Let the listeners come up.
    let deadline = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < deadline, "server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    // Warm up, then measure.
    std::thread::sleep(Duration::from_secs(1));
    let start_jobs = jobs.load(Ordering::Relaxed);
    let t0 = Instant::now();
    let until = t0 + Duration::from_secs(secs);
    let clients: Vec<_> = (0..CLIENTS)
        .map(|_| std::thread::spawn(move || client(addr, until)))
        .collect();
    let mut rtts: Vec<u32> = clients.into_iter().flat_map(|c| c.join().unwrap()).collect();
    let elapsed = t0.elapsed().as_secs_f64();
    let done = jobs.load(Ordering::Relaxed) - start_jobs;
    rtts.sort_unstable();
    let q = |p: f64| rtts[((rtts.len() as f64 * p) as usize).min(rtts.len() - 1)];
    println!(
        "{scenario},{},{},{},{},{},{:.0}",
        rtts.len(),
        q(0.50),
        q(0.99),
        q(0.999),
        rtts[rtts.len() - 1],
        done as f64 / elapsed
    );
    // Printed first: stopping makes the server end the process.
    stop.store(true, Ordering::Relaxed);
    drop(server);
    std::process::exit(0);
}

/// One connection: a 64-byte ping every millisecond, timing each echo.
fn client(addr: SocketAddr, until: Instant) -> Vec<u32> {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    let mut buf = [7u8; PING];
    let mut out = Vec::with_capacity(20_000);
    let mut next = Instant::now();
    while Instant::now() < until {
        let t = Instant::now();
        s.write_all(&buf).unwrap();
        s.read_exact(&mut buf).unwrap();
        out.push(t.elapsed().as_micros() as u32);
        next += Duration::from_millis(1);
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        } else {
            next = Instant::now();
        }
    }
    out
}

fn giong_server(addr: SocketAddr, scenario: String, stop: Arc<AtomicBool>, jobs: Arc<AtomicU64>) {
    let workers = cpus();
    giong::Builder::new()
        .workers(workers)
        .build()
        .unwrap()
        .run(move |_| {
            let (scenario, stop, jobs) = (scenario.clone(), stop.clone(), jobs.clone());
            async move {
                // One job in flight per worker = one per CPU.
                match scenario.as_str() {
                    "idle" => {}
                    "inline" => {
                        let (stop, jobs) = (stop.clone(), jobs.clone());
                        giong::spawn(async move {
                            while !stop.load(Ordering::Relaxed) {
                                black_box(job());
                                jobs.fetch_add(1, Ordering::Relaxed);
                                giong::yield_now().await;
                            }
                        });
                    }
                    "lane" | "lane-nice0" => {
                        let undo = scenario == "lane-nice0";
                        let (stop, jobs) = (stop.clone(), jobs.clone());
                        giong::spawn(async move {
                            while !stop.load(Ordering::Relaxed) {
                                let r = giong::spawn_compute(move || {
                                    if undo {
                                        // SAFETY: plain syscall on the calling thread.
                                        let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 0) };
                                        assert_eq!(rc, 0, "lane-nice0 needs root or CAP_SYS_NICE");
                                    }
                                    job()
                                })
                                .await
                                .unwrap();
                                black_box(r);
                                jobs.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    }
                    other => panic!("unknown scenario {other}"),
                }
                let listener = giong::net::TcpListener::bind(addr).unwrap();
                loop {
                    let stream = match listener.accept().await {
                        Ok(s) => s,
                        Err(e) => {
                            eprintln!("accept: {e}");
                            continue;
                        }
                    };
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = stream.set_nodelay(true);
                    giong::spawn(async move {
                        loop {
                            let Ok(data) = stream.recv().await else { return };
                            if data.is_empty() {
                                return;
                            }
                            let reply = data.to_vec();
                            drop(data);
                            if stream.write_all(reply).await.0.is_err() {
                                return;
                            }
                        }
                    });
                }
                std::process::exit(0);
            }
        })
        .unwrap();
}

fn tokio_server(addr: SocketAddr, load: bool, stop: Arc<AtomicBool>, jobs: Arc<AtomicU64>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let workers = cpus();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        if load {
            for _ in 0..workers {
                let (stop, jobs) = (stop.clone(), jobs.clone());
                tokio::spawn(async move {
                    while !stop.load(Ordering::Relaxed) {
                        black_box(tokio::task::spawn_blocking(job).await.unwrap());
                        jobs.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        }
        let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let _ = s.set_nodelay(true);
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                loop {
                    match s.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            if s.write_all(&buf[..n]).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
}
