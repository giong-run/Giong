//! Where balanced listeners put new connections when the workers are busy
//! in uneven ways: see `bench/placement.sh` for the method.
//!
//!     bench-placement server <workers> <port>
//!     bench-placement skew <port> <seconds> <gap-us>
//!     bench-placement mixed <port> <seconds> <seed>
//!
//! The protocol is 8-byte frames: a request holds the microseconds of CPU
//! work the server is to do for it (u32, little endian) and an id; the
//! server does the work on the worker that owns the connection and sends
//! the frame back.

use std::io::{Read, Write};
use std::net::TcpStream as StdStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let num = |i: usize| -> u64 { args[i].parse().expect("a number") };
    match args.first().map(String::as_str) {
        Some("server") => server(num(1) as usize, num(2) as u16),
        Some("skew") => skew(num(1) as u16, num(2), num(3)),
        Some("mixed") => mixed(num(1) as u16, num(2), num(3)),
        _ => eprintln!("usage: bench-placement server|skew|mixed ..."),
    }
}

// ---------------------------------------------------------------- server

fn server(workers: usize, port: u16) {
    let counts: Arc<Vec<AtomicU64>> = Arc::new((0..workers).map(|_| AtomicU64::new(0)).collect());
    let report = counts.clone();
    let rt = giong::Builder::new()
        .workers(workers)
        .shutdown_on_signals(true)
        .build()
        .expect("runtime");
    let handle = rt.handle();
    rt.run(move |worker| {
        let counts = counts.clone();
        async move {
            let addr = ("127.0.0.1", port);
            // PLAIN=1: plain SO_REUSEPORT listeners, for comparison.
            let listener = if std::env::var_os("PLAIN").is_some_and(|v| v == "1") {
                giong::net::TcpListener::bind(addr)
            } else {
                giong::net::TcpListener::bind_balanced(addr)
            }
            .expect("bind");
            while let Some(conn) = giong::until_shutdown(listener.accept()).await {
                let Ok(stream) = conn else { continue };
                let _ = stream.set_nodelay(true);
                counts[worker].fetch_add(1, Ordering::Relaxed);
                drop(giong::spawn(serve(stream, worker as u32)));
            }
        }
    })
    .expect("run");
    let accepted: Vec<u64> = report.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let idle: Vec<u128> = (0..workers)
        .map(|w| handle.pulse(w).idle.as_millis())
        .collect();
    eprintln!("accepted per worker {accepted:?}, idle ms per worker {idle:?}");
}

async fn serve(stream: giong::net::TcpStream, worker: u32) {
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let Ok(data) = stream.recv().await else {
            return;
        };
        if data.is_empty() {
            return;
        }
        pending.extend_from_slice(&data);
        drop(data);
        let whole = pending.len() / 8 * 8;
        if whole == 0 {
            continue;
        }
        for frame in pending[..whole].chunks_exact(8) {
            let us = u32::from_le_bytes(frame[..4].try_into().unwrap());
            work(Duration::from_micros(u64::from(us)));
        }
        let mut reply: Vec<u8> = pending.drain(..whole).collect();
        // The id field of each reply says which worker served it.
        for frame in reply.chunks_exact_mut(8) {
            frame[4..].copy_from_slice(&worker.to_le_bytes());
        }
        if stream.write_all(reply).await.0.is_err() {
            return;
        }
    }
}

/// CPU work for `d`, on this thread.
fn work(d: Duration) {
    let start = Instant::now();
    while start.elapsed() < d {
        std::hint::spin_loop();
    }
}

// ---------------------------------------------------------------- client

fn connect(port: u16) -> StdStream {
    let s = StdStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_nodelay(true).unwrap();
    s
}

/// One request and its reply; returns the round trip and the worker that
/// served it.
fn call(s: &mut StdStream, us: u32) -> std::io::Result<(Duration, u32)> {
    let mut frame = [0u8; 8];
    frame[..4].copy_from_slice(&us.to_le_bytes());
    let start = Instant::now();
    s.write_all(&frame)?;
    s.read_exact(&mut frame)?;
    Ok((
        start.elapsed(),
        u32::from_le_bytes(frame[4..].try_into().unwrap()),
    ))
}

/// A connection that asks for `us` of work back to back until `stop`,
/// counting answered requests in `answered`.
fn hot(
    port: u16,
    us: u32,
    stop: Arc<AtomicBool>,
    answered: Arc<AtomicU64>,
) -> thread::JoinHandle<()> {
    let mut s = connect(port);
    thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if call(&mut s, us).is_err() {
                break;
            }
            answered.fetch_add(1, Ordering::Relaxed);
        }
    })
}

fn percentile(sorted: &[Duration], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i].as_secs_f64() * 1e6
}

/// The skewed case: one hot connection saturates whichever worker gets it,
/// then 300 idle connections arrive (counts send them to the other
/// workers), then new short connections keep arriving: by counts they go
/// to the saturated worker, which has the fewest tasks.
///
/// With `gap_us` 0 each probe sends its next request as soon as the last
/// one is answered. Otherwise each probe sends one request every `gap_us`
/// on a fixed schedule, or at once when it is behind. Latency is the round
/// trip of each request, from when it was sent. (Counting from when it was
/// due instead measures the client: on 2 vCPUs its sleeps overshoot by
/// milliseconds now and then.)
fn skew(port: u16, secs: u64, gap_us: u64) {
    let stop = Arc::new(AtomicBool::new(false));
    let answered = Arc::new(AtomicU64::new(0));
    let hot_conn = hot(port, 200, stop.clone(), answered.clone());
    thread::sleep(Duration::from_millis(200));
    let idle: Vec<StdStream> = (0..300)
        .map(|_| {
            thread::sleep(Duration::from_millis(1));
            connect(port)
        })
        .collect();
    thread::sleep(Duration::from_millis(200));

    // Probes: 4 threads, each opening a connection, making 20 requests of
    // 20 us of work back to back, closing it, and starting again.
    let lat = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicU64::new(0));
    let probe_stop = Arc::new(AtomicBool::new(false));
    let probes: Vec<_> = (0..4)
        .map(|_| {
            let (lat, done, stop) = (lat.clone(), done.clone(), probe_stop.clone());
            thread::spawn(move || {
                let mut mine = Vec::new();
                let gap = Duration::from_micros(gap_us);
                let mut due = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    let mut s = connect(port);
                    for i in 0..20 {
                        if gap_us > 0 {
                            due += gap;
                            let now = Instant::now();
                            if due > now {
                                thread::sleep(due - now);
                            }
                        }
                        match call(&mut s, 20) {
                            Ok((d, w)) => mine.push((d, w, i == 0)),
                            Err(_) => break,
                        }
                    }
                    done.fetch_add(1, Ordering::Relaxed);
                }
                lat.lock().unwrap().extend(mine);
            })
        })
        .collect();
    let (t0, hot_at_start) = (Instant::now(), answered.load(Ordering::Relaxed));
    thread::sleep(Duration::from_secs(secs));
    let hot_n = answered.load(Ordering::Relaxed) - hot_at_start;
    let hot_secs = t0.elapsed().as_secs_f64();
    probe_stop.store(true, Ordering::Relaxed);
    for p in probes {
        p.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    hot_conn.join().unwrap();
    drop(idle);
    let all = lat.lock().unwrap().clone();
    let mut lat: Vec<Duration> = all.iter().map(|x| x.0).collect();
    lat.sort();
    let n = lat.len();
    println!(
        "probe_rps={:.0} p50_us={:.0} p99_us={:.0} p999_us={:.0} max_us={:.0} probe_conns={} hot_rps={:.0}",
        n as f64 / secs as f64,
        percentile(&lat, 0.50),
        percentile(&lat, 0.99),
        percentile(&lat, 0.999),
        percentile(&lat, 1.0),
        done.load(Ordering::Relaxed),
        hot_n as f64 / hot_secs,
    );
    // The same split by the worker that served each request, and for the
    // first request of each connection (which waits for the connection to
    // be accepted, and handed over if it is).
    let workers = all.iter().map(|x| x.1).max().unwrap_or(0) + 1;
    for w in 0..workers {
        let mut l: Vec<Duration> = all.iter().filter(|x| x.1 == w).map(|x| x.0).collect();
        l.sort();
        println!(
            "  worker {w}: {} requests, p50_us={:.0} p99_us={:.0}",
            l.len(),
            percentile(&l, 0.50),
            percentile(&l, 0.99)
        );
    }
    let mut first: Vec<Duration> = all.iter().filter(|x| x.2).map(|x| x.0).collect();
    first.sort();
    println!(
        "  first request of a connection: p50_us={:.0} p99_us={:.0}",
        percentile(&first, 0.50),
        percentile(&first, 0.99)
    );
}

/// Mixed arrivals: 24 connections arrive 10 ms apart in a random order
/// (from `seed`); 6 are hot (100 us of work back to back), 18 are light (one
/// request of 20 us every 20 ms). Then all run for `secs`. Where the hot
/// ones land decides how much work gets done and how long light requests
/// wait behind them.
fn mixed(port: u16, secs: u64, seed: u64) {
    let mut order: Vec<bool> = (0..24).map(|i| i < 6).collect();
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..order.len()).rev() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        order.swap(i, (x % (i as u64 + 1)) as usize);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let measuring = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();
    for is_hot in order {
        let mut s = connect(port);
        let (stop, measuring) = (stop.clone(), measuring.clone());
        // Every connection starts working at once, so that later choices
        // can see the hot ones; only what happens after the last one is
        // open is counted.
        threads.push(thread::spawn(move || {
            let (mut n, mut lat, mut at) = (0u64, Vec::new(), 0);
            while !stop.load(Ordering::Relaxed) {
                let us = if is_hot { 100 } else { 20 };
                let Ok((d, w)) = call(&mut s, us) else { break };
                at = w;
                if !measuring.load(Ordering::Relaxed) {
                } else if is_hot {
                    n += 1;
                } else {
                    lat.push(d);
                }
                if !is_hot {
                    thread::sleep(Duration::from_millis(20));
                }
            }
            (n, lat, is_hot, at)
        }));
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(100));
    measuring.store(true, Ordering::Relaxed);
    let t0 = Instant::now();
    thread::sleep(Duration::from_secs(secs));
    stop.store(true, Ordering::Relaxed);
    let (mut hot_n, mut lat) = (0, Vec::new());
    let mut placed = [[0u32; 2]; 8];
    for t in threads {
        let (n, l, is_hot, at) = t.join().unwrap();
        hot_n += n;
        lat.extend(l);
        placed[(at as usize).min(7)][usize::from(!is_hot)] += 1;
    }
    let el = t0.elapsed().as_secs_f64();
    lat.sort();
    println!(
        "hot_rps={:.0} light_p50_us={:.0} light_p99_us={:.0} light_p999_us={:.0} light_max_us={:.0}",
        hot_n as f64 / el,
        percentile(&lat, 0.50),
        percentile(&lat, 0.99),
        percentile(&lat, 0.999),
        percentile(&lat, 1.0),
    );
    // Where the connections ended up: hot/light on each worker.
    let workers = placed
        .iter()
        .rposition(|p| p != &[0, 0])
        .map_or(1, |i| i + 1);
    for (w, [hot, light]) in placed.iter().take(workers).enumerate() {
        println!("  worker {w}: {hot} hot, {light} light");
    }
}
