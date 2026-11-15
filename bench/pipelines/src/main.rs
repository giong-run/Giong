//! Micro-benchmarks for the io_uring pipeline features: cross-worker
//! wake-ups (`ping`), one bulk TCP stream into `recv` (`bulk`) and a TCP
//! proxy that copies through user space or splices (`proxy`).
//!
//!   bench-pipelines <case> [args]
//!
//! Every case prints one CSV line: case,variant,ops,secs,cpu_us_per_op,extra.
//! CPU is the time the giong worker threads spent, user and kernel, from
//! getrusage(RUSAGE_THREAD), so the load generator is not counted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn thread_cpu_us() -> u64 {
    // SAFETY: getrusage fills the struct it is given.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn variant() -> String {
    std::env::var("VARIANT").unwrap_or_else(|_| "default".into())
}

static P: std::sync::Mutex<Option<(u64, u64)>> = std::sync::Mutex::new(None);
thread_local! { static LAT: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) }; }

type Side = (giong::sync::mpsc::Sender<u64>, giong::sync::mpsc::Receiver<u64>);

/// Two workers bounce a counter over two channels `n` times. Each side
/// waits in between, so every message wakes a worker that sleeps (or is
/// about to) in the kernel: the cross-worker wake-up path.
fn ping(n: u64) {
    ping_with(n, 0)
}

/// `busy` tasks on worker 0 keep its run queue full: each spins for about
/// 2 µs, then yields. Shows what the wake-up path costs the latency of a
/// worker that has other work.
fn ping_with(n: u64, busy: usize) {
    let (tx_a, rx_a) = giong::sync::mpsc::channel::<u64>();
    let (tx_b, rx_b) = giong::sync::mpsc::channel::<u64>();
    let sides: Arc<[std::sync::Mutex<Option<Side>>; 2]> = Arc::new([
        std::sync::Mutex::new(Some((tx_a, rx_b))),
        std::sync::Mutex::new(Some((tx_b, rx_a))),
    ]);
    let cpu = Arc::new(AtomicU64::new(0));
    let c = cpu.clone();
    let start = Instant::now();
    giong::Builder::new()
        .workers(2)
        .build()
        .unwrap()
        .run(move |w| {
            let (tx, mut rx) = sides[w].lock().unwrap().take().unwrap();
            let c = c.clone();
            async move {
                let t0 = thread_cpu_us();
                if w == 0 {
                    let stop = std::rc::Rc::new(std::cell::Cell::new(false));
                    for _ in 0..busy {
                        let stop = stop.clone();
                        giong::spawn(async move {
                            while !stop.get() {
                                let t = Instant::now();
                                while t.elapsed().as_nanos() < 2000 {
                                    std::hint::spin_loop();
                                }
                                giong::yield_now().await;
                            }
                        });
                    }
                    let mut lat = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        let t = Instant::now();
                        tx.send(i).unwrap();
                        assert_eq!(rx.recv().await, Some(i));
                        lat.push(t.elapsed().as_nanos() as u64);
                    }
                    stop.set(true);
                    lat.sort_unstable();
                    let q = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)];
                    eprintln!("p50={} p99={} p999={}", q(0.5), q(0.99), q(0.999));
                    LAT.with(|l| l.set((q(0.5), q(0.99))));
                    P.lock().unwrap().replace((q(0.5), q(0.99)));
                } else {
                    while let Some(i) = rx.recv().await {
                        if tx.send(i).is_err() {
                            break;
                        }
                        if i + 1 == n {
                            break;
                        }
                    }
                }
                c.fetch_add(thread_cpu_us() - t0, Ordering::Relaxed);
            }
        })
        .unwrap();
    let secs = start.elapsed().as_secs_f64();
    let (p50, p99) = P.lock().unwrap().unwrap_or_default();
    println!(
        "ping-busy{busy},{},{n},{secs:.3},{:.3},ns_per_round_trip={:.0};p50_ns={p50};p99_ns={p99}",
        variant(),
        cpu.load(Ordering::Relaxed) as f64 / n as f64,
        secs * 1e9 / n as f64
    );
}

/// `conns` plain std threads each write `mib` MiB in 64 KiB writes to one
/// giong worker, which `recv`s and drops the bytes: the receive path of a
/// busy stream (upload, proxy, replication).
static COPIED: AtomicU64 = AtomicU64::new(0);

fn bulk(mib: u64, conns: usize) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let cpu = Arc::new(AtomicU64::new(0));
    let c = cpu.clone();
    let lst = std::sync::Mutex::new(Some(listener));
    let start = Arc::new(std::sync::Mutex::new(None::<Instant>));
    let st = start.clone();
    let writers: Vec<_> = (0..conns)
        .map(|_| {
            std::thread::spawn(move || {
                let mut s = std::net::TcpStream::connect(addr).unwrap();
                let chunk = vec![7u8; 64 * 1024];
                for _ in 0..mib * 16 {
                    std::io::Write::write_all(&mut s, &chunk).unwrap();
                }
            })
        })
        .collect();
    giong::Builder::new()
        .workers(1)
        .build()
        .unwrap()
        .run(move |_| {
            let l = lst.lock().unwrap().take().unwrap();
            let c = c.clone();
            let st = st.clone();
            async move {
                let l = giong::net::TcpListener::from_std(l);
                let t0 = thread_cpu_us();
                *st.lock().unwrap() = Some(Instant::now());
                let mut tasks = Vec::new();
                for _ in 0..conns {
                    let s = l.accept().await.unwrap();
                    tasks.push(giong::spawn(async move {
                        let mut n = 0u64;
                        loop {
                            let b = s.recv().await.unwrap();
                            if b.is_empty() {
                                break;
                            }
                            n += b.len() as u64;
                        }
                        n
                    }));
                }
                let mut total = 0;
                for t in tasks {
                    total += t.await.unwrap();
                }
                assert_eq!(total, mib * conns as u64 * 1024 * 1024);
                c.fetch_add(thread_cpu_us() - t0, Ordering::Relaxed);
                COPIED.store(giong::observe::worker_stats().recv_copied, Ordering::Relaxed);
            }
        })
        .unwrap();
    let secs = start.lock().unwrap().unwrap().elapsed().as_secs_f64();
    for w in writers {
        w.join().unwrap();
    }
    let gib = (mib * conns as u64) as f64 / 1024.0;
    println!(
        "bulk-c{conns},{},{},{secs:.3},{:.1},gib_per_s={:.2};copied_mib={}",
        variant(),
        mib * conns as u64,
        cpu.load(Ordering::Relaxed) as f64 / gib,
        gib / secs,
        COPIED.load(Ordering::Relaxed) >> 20
    );
}

/// `conns` std threads each write `mib` MiB through a giong proxy (one
/// worker) to a std sink thread per connection. The proxy forwards with
/// `recv` + `write_all` (PROXY_MODE=copy, the default) or `splice_to`
/// (PROXY_MODE=splice). CPU is the proxy worker's.
fn proxy(mib: u64, conns: usize) {
    let mode = std::env::var("PROXY_MODE").unwrap_or_else(|_| "copy".into());
    let splice = mode == "splice";
    let read = mode == "read";
    let sink = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let sink_addr = sink.local_addr().unwrap();
    let sink_thread = std::thread::spawn(move || {
        let readers: Vec<_> = (0..conns)
            .map(|_| {
                let (mut s, _) = sink.accept().unwrap();
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 256 * 1024];
                    let mut n = 0u64;
                    loop {
                        let k = std::io::Read::read(&mut s, &mut buf).unwrap();
                        if k == 0 {
                            break n;
                        }
                        n += k as u64;
                    }
                })
            })
            .collect();
        readers.into_iter().map(|r| r.join().unwrap()).sum::<u64>()
    });
    let front = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let front_addr = front.local_addr().unwrap();
    let front = std::sync::Mutex::new(Some(front));
    let cpu = Arc::new(AtomicU64::new(0));
    let c = cpu.clone();
    let writers: Vec<_> = (0..conns)
        .map(|_| {
            std::thread::spawn(move || {
                let mut s = std::net::TcpStream::connect(front_addr).unwrap();
                let chunk = vec![7u8; 64 * 1024];
                for _ in 0..mib * 16 {
                    std::io::Write::write_all(&mut s, &chunk).unwrap();
                }
            })
        })
        .collect();
    let start = Instant::now();
    giong::Builder::new()
        .workers(1)
        .build()
        .unwrap()
        .run(move |_| {
            let l = front.lock().unwrap().take().unwrap();
            let c = c.clone();
            async move {
                let l = giong::net::TcpListener::from_std(l);
                let t0 = thread_cpu_us();
                let mut tasks = Vec::new();
                for _ in 0..conns {
                    let client = l.accept().await.unwrap();
                    tasks.push(giong::spawn(async move {
                        let up = giong::net::TcpStream::connect(sink_addr).await.unwrap();
                        let n = if splice {
                            client.splice_to(&up, u64::MAX).await.unwrap()
                        } else if read {
                            // Own buffer: one copy in, one copy out.
                            let mut n = 0u64;
                            let mut buf = Vec::with_capacity(256 * 1024);
                            loop {
                                buf.clear();
                                let (r, b) = client.read(buf).await;
                                let k = r.unwrap();
                                if k == 0 {
                                    break;
                                }
                                n += k as u64;
                                let (w, b) = up.write_all(b).await;
                                w.unwrap();
                                buf = b;
                            }
                            n
                        } else {
                            // Pool buffers: copied once more into a reused Vec.
                            let mut n = 0u64;
                            let mut buf = Vec::with_capacity(256 * 1024);
                            loop {
                                let b = client.recv().await.unwrap();
                                if b.is_empty() {
                                    break;
                                }
                                n += b.len() as u64;
                                buf.clear();
                                buf.extend_from_slice(b.as_slice());
                                drop(b);
                                let (w, v) = up.write_all(buf).await;
                                w.unwrap();
                                buf = v;
                            }
                            n
                        };
                        up.shutdown(std::net::Shutdown::Write).unwrap();
                        n
                    }));
                }
                let mut total = 0;
                for t in tasks {
                    total += t.await.unwrap();
                }
                assert_eq!(total, mib * conns as u64 * 1024 * 1024);
                c.fetch_add(thread_cpu_us() - t0, Ordering::Relaxed);
            }
        })
        .unwrap();
    let secs = start.elapsed().as_secs_f64();
    for w in writers {
        w.join().unwrap();
    }
    assert_eq!(sink_thread.join().unwrap(), mib * conns as u64 * 1024 * 1024);
    let gib = (mib * conns as u64) as f64 / 1024.0;
    println!(
        "proxy-c{conns},{},{},{secs:.3},{:.1},gib_per_s={:.2}",
        variant(),
        mib * conns as u64,
        cpu.load(Ordering::Relaxed) as f64 / gib,
        gib / secs
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let case = args.get(1).map(String::as_str).unwrap_or("ping");
    let n: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    match case {
        "ping" => ping(n),
        "proxy" => proxy(n, args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1)),
        "bulk" => bulk(n, args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1)),
        "ping-busy" => ping_with(n, args.get(3).and_then(|s| s.parse().ok()).unwrap_or(32)),
        _ => panic!("unknown case {case}"),
    }
}
