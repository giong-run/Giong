//! Load generator for bench/grpc.sh on tonic's client.
//!
//!   load <port> <workload> <connections> <tasks> <seconds> [threads]
//!
//! The same workloads, numbers and output as giong's
//! (bench/grpc-giong/src/bin/load.rs). `connections` channels, each its
//! own HTTP/2 connection; task `t` uses channel `t % connections`. With 1
//! thread (the default) a current-thread Tokio runtime; with more, the
//! multi-thread runtime, to load a server from several cores.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bench_grpc_tonic::pb::bench_client::BenchClient;
use bench_grpc_tonic::pb::{DownloadRequest, Echo};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, Endpoint};

/// CPU time this process has used, in clock ticks (user + system).
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat
        .rsplit(')')
        .next()
        .unwrap()
        .split_whitespace()
        .collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args[1].parse().unwrap();
    let workload = args[2].clone();
    let connections: usize = args[3].parse().unwrap();
    let tasks: usize = args[4].parse().unwrap();
    let secs: u64 = args[5].parse().unwrap();
    let threads: usize = args.get(6).map_or(1, |t| t.parse().unwrap());
    let runtime = if threads == 1 {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .enable_all()
            .build()
    }
    .unwrap();

    runtime.block_on(async move {
        let mut clients: Vec<BenchClient<Channel>> = Vec::new();
        for _ in 0..connections {
            let channel = Endpoint::from_shared(format!("http://127.0.0.1:{port}"))
                .unwrap()
                .tcp_nodelay(true)
                .connect()
                .await
                .unwrap();
            // 10,000-byte payloads fit the default 4 MiB limit.
            clients.push(BenchClient::new(channel));
        }
        let measuring = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicU64::new(0));
        let ready = Arc::new(AtomicUsize::new(0));
        let all = Arc::new(Mutex::new(Vec::<Vec<u32>>::new()));
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (mut client, workload, measuring, stop, count, ready, all) = (
                    clients[t % connections].clone(),
                    workload.clone(),
                    measuring.clone(),
                    stop.clone(),
                    count.clone(),
                    ready.clone(),
                    all.clone(),
                );
                tokio::spawn(async move {
                    let payload = vec![b'p'; if workload == "unary10k" { 10_000 } else { 100 }];
                    let mut samples = Vec::<u32>::with_capacity(1 << 16);
                    let mut id = 0u64;
                    macro_rules! record {
                        ($start:expr, $n:expr) => {
                            if measuring.load(Ordering::Relaxed) {
                                samples.push($start.elapsed().as_micros() as u32);
                                count.fetch_add($n, Ordering::Relaxed);
                            }
                        };
                    }
                    match workload.as_str() {
                        "unary" | "unary10k" => {
                            while !stop.load(Ordering::Relaxed) {
                                id += 1;
                                let start = Instant::now();
                                let request = Echo {
                                    payload: payload.clone(),
                                    id,
                                };
                                let reply = client.unary(request).await.unwrap();
                                assert_eq!(reply.get_ref().id, id);
                                record!(start, 1);
                            }
                        }
                        "duplex" | "hold" => {
                            let (tx, rx) = tokio::sync::mpsc::channel(1);
                            let mut responses = client
                                .duplex(ReceiverStream::new(rx))
                                .await
                                .unwrap()
                                .into_inner();
                            let mut first = true;
                            while !stop.load(Ordering::Relaxed) {
                                id += 1;
                                let start = Instant::now();
                                let request = Echo {
                                    payload: payload.clone(),
                                    id,
                                };
                                tx.send(request).await.unwrap();
                                let received = responses.message().await.unwrap().unwrap();
                                assert_eq!(received.id, id);
                                record!(start, 1);
                                if workload == "hold" {
                                    if first {
                                        ready.fetch_add(1, Ordering::Relaxed);
                                        first = false;
                                    }
                                    std::future::pending::<()>().await;
                                }
                            }
                        }
                        "download" => {
                            while !stop.load(Ordering::Relaxed) {
                                let start = Instant::now();
                                let request = DownloadRequest {
                                    count: 1000,
                                    size: 100,
                                };
                                let mut responses =
                                    client.download(request).await.unwrap().into_inner();
                                let mut n = 0;
                                while let Some(received) = responses.message().await.unwrap() {
                                    n += u64::from(received.payload.len() == 100);
                                }
                                assert_eq!(n, 1000);
                                record!(start, n);
                            }
                        }
                        w => panic!("unknown workload {w}"),
                    }
                    all.lock().unwrap().push(samples);
                })
            })
            .collect();
        if workload == "hold" {
            while ready.load(Ordering::Relaxed) < tasks {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            println!("ready");
            std::future::pending::<()>().await;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        measuring.store(true, Ordering::Relaxed);
        let ticks = cpu_ticks();
        let begin = Instant::now();
        tokio::time::sleep(Duration::from_secs(secs)).await;
        measuring.store(false, Ordering::Relaxed);
        let elapsed = begin.elapsed().as_secs_f64();
        let hz: f64 = std::env::var("CLK_TCK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100.0);
        let cpu = (cpu_ticks() - ticks) as f64 * 100.0 / (hz * elapsed);
        stop.store(true, Ordering::Relaxed);
        // Each task ends after the call it is in.
        for handle in handles {
            let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
        }
        let mut s: Vec<u32> = all.lock().unwrap().drain(..).flatten().collect();
        s.sort_unstable();
        let pct = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)];
        println!(
            "ops={:.0} p50_us={} p99_us={} cpu={:.0}",
            count.load(Ordering::Relaxed) as f64 / elapsed,
            pct(0.50),
            pct(0.99),
            cpu,
        );
        std::process::exit(0);
    });
}
