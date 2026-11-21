//! Load generator for bench/grpc.sh on giong-grpc's client, on one giong
//! worker.
//!
//!   load <port> <workload> <connections> <tasks> <seconds>
//!
//! `connections` channels, each its own HTTP/2 connection; task `t` uses
//! channel `t % connections`. Workloads:
//!
//! * `unary`: back-to-back unary calls with a 100-byte payload;
//! * `unary10k`: the same with 10,000 bytes;
//! * `duplex`: one bidirectional stream per task, a 100-byte message sent
//!   and its echo awaited, back to back;
//! * `download`: server streams of 1,000 messages of 100 bytes, back to
//!   back; counts messages;
//! * `hold`: opens one bidirectional stream per task, exchanges one
//!   message on each, prints `ready` and keeps them open until killed.
//!
//! Prints `ops=<per second> p50_us=<..> p99_us=<..> cpu=<%>` for the
//! measured part, after a warm-up of 2 seconds. A latency is that of one
//! call, of one round trip on a stream, or of one whole download.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use bench_grpc_giong::pb::bench::v1::bench::BenchClient;
use bench_grpc_giong::pb::bench::v1::{DownloadRequest, Echo};
use giong_grpc::{CallOptions, Channel};

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
    // Long enough for the whole run: the default would end the streams.
    let options = || CallOptions::new().timeout(Duration::from_secs(24 * 3600));

    giong::block_on(async move {
        let clients: Vec<BenchClient> = (0..connections)
            .map(|_| BenchClient::new(Channel::new(&format!("http://127.0.0.1:{port}")).unwrap()))
            .collect();
        let measuring = Rc::new(Cell::new(false));
        let stop = Rc::new(Cell::new(false));
        let samples = Rc::new(RefCell::new(Vec::<u32>::with_capacity(1 << 24)));
        let count = Rc::new(Cell::new(0u64));
        let ready = Rc::new(Cell::new(0usize));
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (client, workload, measuring, stop, samples, count, ready) = (
                    clients[t % connections].clone(),
                    workload.clone(),
                    measuring.clone(),
                    stop.clone(),
                    samples.clone(),
                    count.clone(),
                    ready.clone(),
                );
                giong::spawn(async move {
                    let payload = vec![b'p'; if workload == "unary10k" { 10_000 } else { 100 }];
                    let mut id = 0u64;
                    let record = |start: Instant, n: u64| {
                        if measuring.get() {
                            samples
                                .borrow_mut()
                                .push(start.elapsed().as_micros() as u32);
                            count.set(count.get() + n);
                        }
                    };
                    match workload.as_str() {
                        "unary" | "unary10k" => {
                            while !stop.get() {
                                id += 1;
                                let start = Instant::now();
                                let request = Echo {
                                    payload: &payload,
                                    id,
                                    ..Default::default()
                                };
                                let reply = client.unary(&request, options()).await.unwrap();
                                assert_eq!(reply.message().unwrap().id(), id);
                                record(start, 1);
                            }
                        }
                        "duplex" | "hold" => {
                            let (mut requests, mut responses) = client.duplex(options());
                            let mut first = true;
                            while !stop.get() {
                                id += 1;
                                let start = Instant::now();
                                let request = Echo {
                                    payload: &payload,
                                    id,
                                    ..Default::default()
                                };
                                requests.send(&request).await.unwrap();
                                let received = responses.message().await.unwrap().unwrap();
                                assert_eq!(received.message().unwrap().id(), id);
                                record(start, 1);
                                if workload == "hold" {
                                    if first {
                                        ready.set(ready.get() + 1);
                                        first = false;
                                    }
                                    std::future::pending::<()>().await;
                                }
                            }
                        }
                        "download" => {
                            let request = DownloadRequest {
                                count: 1000,
                                size: 100,
                                ..Default::default()
                            };
                            while !stop.get() {
                                let start = Instant::now();
                                let mut responses =
                                    client.download(&request, options()).await.unwrap();
                                let mut n = 0;
                                while let Some(received) = responses.message().await.unwrap() {
                                    n += u64::from(
                                        received.message().unwrap().payload().len() == 100,
                                    );
                                }
                                assert_eq!(n, 1000);
                                record(start, n);
                            }
                        }
                        w => panic!("unknown workload {w}"),
                    }
                })
            })
            .collect();
        if workload == "hold" {
            while ready.get() < tasks {
                giong::time::sleep(Duration::from_millis(50)).await;
            }
            println!("ready");
            std::future::pending::<()>().await;
        }
        giong::time::sleep(Duration::from_secs(2)).await;
        measuring.set(true);
        let ticks = cpu_ticks();
        let begin = Instant::now();
        giong::time::sleep(Duration::from_secs(secs)).await;
        measuring.set(false);
        let elapsed = begin.elapsed().as_secs_f64();
        let hz: f64 = std::env::var("CLK_TCK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(100.0);
        let cpu = (cpu_ticks() - ticks) as f64 * 100.0 / (hz * elapsed);
        stop.set(true);
        // Streams in flight are dropped with the process.
        drop(handles);
        let mut s = samples.borrow_mut();
        s.sort_unstable();
        let pct = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)];
        println!(
            "ops={:.0} p50_us={} p99_us={} cpu={:.0}",
            count.get() as f64 / elapsed,
            pct(0.50),
            pct(0.99),
            cpu,
        );
        std::process::exit(0);
    });
}
