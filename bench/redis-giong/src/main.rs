//! Load generator for bench/redis.sh: giong-redis on one giong worker.
//!
//!   bench-redis-giong <port> <workload> <tasks> <seconds>
//!
//! One `Pool` of at most POOL_SIZE connections (1 by default) shared by
//! every task. Workloads: `get` (GET of a 64-byte value), `set` (SET of a
//! 64-byte value), `get8k` (GET of an 8 KiB value), `mget` (MGET of 10
//! 64-byte values). Prints `ops=<per second> p50_us=<..> p99_us=<..>
//! cpu=<%>` for the measured part, after a warm-up of 2 seconds.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong_redis::{Config, Pool};

/// CPU time this process has used, in clock ticks (user + system).
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap().split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args[1].parse().unwrap();
    let workload = args[2].clone();
    let tasks: usize = args[3].parse().unwrap();
    let secs: u64 = args[4].parse().unwrap();
    let size: usize = std::env::var("POOL_SIZE").map_or(1, |v| v.parse().unwrap());
    let config = Config::new().host("127.0.0.1").port(port);

    giong::block_on(async move {
        let pool = Pool::new(config, size);
        pool.set("bench:64", vec![b'v'; 64]).await.unwrap();
        pool.set("bench:8k", vec![b'v'; 8192]).await.unwrap();
        let keys: Vec<String> = (0..10).map(|i| format!("bench:m{i}")).collect();
        for k in &keys {
            pool.set(k, vec![b'v'; 64]).await.unwrap();
        }
        let keys = Rc::new(keys);
        let measuring = Rc::new(Cell::new(false));
        let stop = Rc::new(Cell::new(false));
        let samples = Rc::new(RefCell::new(Vec::<u32>::with_capacity(1 << 24)));
        let sink = Rc::new(Cell::new(0usize));
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (pool, workload, measuring, stop, samples, sink, keys) = (
                    pool.clone(),
                    workload.clone(),
                    measuring.clone(),
                    stop.clone(),
                    samples.clone(),
                    sink.clone(),
                    keys.clone(),
                );
                giong::spawn(async move {
                    let own = format!("bench:set:{t}");
                    let value = vec![b's'; 64];
                    while !stop.get() {
                        let start = Instant::now();
                        let n = match workload.as_str() {
                            "get" => pool.get::<Vec<u8>>("bench:64").await.unwrap().len(),
                            "get8k" => pool.get::<Vec<u8>>("bench:8k").await.unwrap().len(),
                            "set" => {
                                pool.set(&own, &value).await.unwrap();
                                1
                            }
                            "mget" => pool
                                .mget::<Vec<Vec<u8>>, _>(&keys)
                                .await
                                .unwrap()
                                .len(),
                            w => panic!("unknown workload {w}"),
                        };
                        sink.set(sink.get() + n);
                        if measuring.get() {
                            samples.borrow_mut().push(start.elapsed().as_micros() as u32);
                        }
                    }
                })
            })
            .collect();
        giong::time::sleep(Duration::from_secs(2)).await;
        measuring.set(true);
        let ticks = cpu_ticks();
        let begin = Instant::now();
        giong::time::sleep(Duration::from_secs(secs)).await;
        measuring.set(false);
        let elapsed = begin.elapsed().as_secs_f64();
        let hz: f64 = std::env::var("CLK_TCK").ok().and_then(|v| v.parse().ok()).unwrap_or(100.0);
        let cpu = (cpu_ticks() - ticks) as f64 * 100.0 / (hz * elapsed);
        stop.set(true);
        for h in handles {
            h.await.unwrap();
        }
        let mut s = samples.borrow_mut();
        s.sort_unstable();
        let pct = |p: f64| s[((s.len() as f64 * p) as usize).min(s.len() - 1)];
        println!(
            "ops={:.0} p50_us={} p99_us={} cpu={:.0} sink={} conns={}",
            s.len() as f64 / elapsed,
            pct(0.50),
            pct(0.99),
            cpu,
            sink.get() % 10,
            pool.stats().connections
        );
    });
}
