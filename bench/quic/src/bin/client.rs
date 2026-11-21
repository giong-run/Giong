//! Load generator: quinn on a multi-threaded Tokio runtime, one endpoint
//! (socket and driver task) per client thread so the client is not limited
//! by one endpoint's driver. Prints one `key=value` line.
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

include!("../args.rs");

fn main() {
    let addr: SocketAddr = arg("addr", "127.0.0.1:4433".parse().unwrap());
    let conns: usize = arg("conns", 8);
    let streams: usize = arg("streams", 16);
    let size: u64 = arg("size", 100);
    let warmup: f64 = arg("warmup", 2.0);
    let secs: f64 = arg("secs", 5.0);
    let threads: usize = arg("threads", 3);
    let server_pid: u32 = arg("server-pid", 0);
    // The server runs on another machine: read its CPU time over ssh.
    let server_ssh: String = arg("server-ssh", String::new());
    let ssh = (!server_ssh.is_empty()).then_some(server_ssh);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut endpoints = Vec::new();
        for _ in 0..threads {
            let bind: SocketAddr = if addr.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" }.parse().unwrap();
            let mut ep = quinn::Endpoint::client(bind).unwrap();
            ep.set_default_client_config(bench_quic::client_config());
            endpoints.push(ep);
        }
        let phase = Arc::new(AtomicU8::new(0));
        let reqs = Arc::new(AtomicU64::new(0));
        let bytes = Arc::new(AtomicU64::new(0));
        let errors = Arc::new(AtomicU64::new(0));
        let lat = Arc::new(Mutex::new(Vec::<u32>::new()));
        let mut tasks = Vec::new();
        let mut held = Vec::new();
        for c in 0..conns {
            let conn = endpoints[c % threads]
                .connect(addr, "localhost")
                .unwrap()
                .await
                .expect("handshake");
            held.push(conn.clone());
            for _ in 0..streams {
                let conn = conn.clone();
                let (phase, reqs, bytes, errors, lat) =
                    (phase.clone(), reqs.clone(), bytes.clone(), errors.clone(), lat.clone());
                tasks.push(tokio::spawn(async move {
                    let mut mine = Vec::new();
                    loop {
                        let p = phase.load(Relaxed);
                        if p == 2 {
                            break;
                        }
                        let t0 = Instant::now();
                        let r: Result<u64, Box<dyn std::error::Error + Send + Sync>> = async {
                            let (mut send, mut recv) = conn.open_bi().await?;
                            send.write_all(&size.to_be_bytes()).await?;
                            send.finish()?;
                            let mut n = 0u64;
                            while let Some(chunk) = recv.read_chunk(usize::MAX, true).await? {
                                n += chunk.bytes.len() as u64;
                            }
                            Ok(n)
                        }
                        .await;
                        match r {
                            Ok(n) if p == 1 && phase.load(Relaxed) == 1 => {
                                reqs.fetch_add(1, Relaxed);
                                bytes.fetch_add(n, Relaxed);
                                mine.push(t0.elapsed().as_micros() as u32);
                            }
                            Ok(_) => {}
                            Err(_) => {
                                errors.fetch_add(1, Relaxed);
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        }
                    }
                    lat.lock().unwrap().extend(mine);
                }));
            }
        }
        tokio::time::sleep(Duration::from_secs_f64(warmup)).await;
        let cpu0 = bench_quic::cpu_ticks_on(ssh.as_deref(), server_pid);
        let me0 = bench_quic::cpu_ticks(std::process::id());
        let t0 = Instant::now();
        phase.store(1, Relaxed);
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
        phase.store(2, Relaxed);
        let el = t0.elapsed().as_secs_f64();
        let cpu1 = bench_quic::cpu_ticks_on(ssh.as_deref(), server_pid);
        let me1 = bench_quic::cpu_ticks(std::process::id());
        let (r, b) = (reqs.load(Relaxed), bytes.load(Relaxed));
        // Let the loops notice the stop and hand in their latencies.
        for t in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), t).await;
        }
        let mut l = lat.lock().unwrap().clone();
        l.sort_unstable();
        let pct = |p: f64| l.get(((l.len() as f64 * p) as usize).min(l.len().saturating_sub(1))).copied().unwrap_or(0);
        let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
        let srv_cpu = (cpu1 - cpu0) as f64 / tck;
        // What the client's side of the connections saw of the path: its
        // own packets declared lost, the times it cut its window, and the
        // slowest connection's round-trip time.
        let (mut lost, mut cut, mut rtt) = (0, 0, Duration::ZERO);
        for conn in &held {
            let path = conn.stats().path;
            lost += path.lost_packets;
            cut += path.congestion_events;
            rtt = rtt.max(path.rtt);
        }
        println!(
            "req_s={:.0} gbit_s={:.3} p50_us={} p99_us={} srv_cpu_pct={:.1} srv_us_per_req={:.2} srv_ns_per_kib={:.1} cli_cpu_pct={:.1} errors={} cli_lost_packets={lost} cli_congestion_events={cut} cli_max_rtt_us={}",
            r as f64 / el,
            b as f64 * 8.0 / el / 1e9,
            pct(0.50),
            pct(0.99),
            srv_cpu / el * 100.0,
            if r > 0 { srv_cpu * 1e6 / r as f64 } else { 0.0 },
            if b > 0 { srv_cpu * 1e9 / (b as f64 / 1024.0) } else { 0.0 },
            (me1 - me0) as f64 / tck / el * 100.0,
            errors.load(Relaxed),
            rtt.as_micros(),
        );
        for ep in endpoints {
            ep.close(0u32.into(), b"done");
        }
    });
}
