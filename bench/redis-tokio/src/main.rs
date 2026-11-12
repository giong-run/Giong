//! Load generator for bench/redis.sh: redis-rs and fred on a
//! current-thread Tokio runtime (one core).
//!
//!   bench-redis-tokio <redis-rs|fred> <port> <workload> <tasks> <seconds>
//!
//! redis-rs: POOL_SIZE MultiplexedConnections (1 by default), task t using
//! number t % POOL_SIZE (a clone of it: redis-rs's way of sharing a
//! connection). fred: POOL_SIZE Clients (one connection each, with fred's
//! automatic pipelining, on by default), shared the same way. Workloads and
//! output as bench-redis-giong.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use fred::prelude::{ClientLike, KeysInterface};

fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap().split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

#[derive(Clone)]
enum Conn {
    RedisRs(redis::aio::MultiplexedConnection),
    Fred(fred::clients::Client),
}

impl Conn {
    async fn run(&mut self, workload: &str, own: &str, value: &[u8], keys: &[String]) -> usize {
        match self {
            Conn::RedisRs(c) => match workload {
                "get" | "get8k" => {
                    let key = if workload == "get" { "bench:64" } else { "bench:8k" };
                    let v: Vec<u8> = redis::cmd("GET").arg(key).query_async(c).await.unwrap();
                    v.len()
                }
                "set" => {
                    let () = redis::cmd("SET").arg(own).arg(value).query_async(c).await.unwrap();
                    1
                }
                "mget" => {
                    let v: Vec<Vec<u8>> = redis::cmd("MGET").arg(keys).query_async(c).await.unwrap();
                    v.len()
                }
                w => panic!("unknown workload {w}"),
            },
            Conn::Fred(c) => match workload {
                "get" | "get8k" => {
                    let key = if workload == "get" { "bench:64" } else { "bench:8k" };
                    let v: Vec<u8> = c.get(key).await.unwrap();
                    v.len()
                }
                "set" => {
                    let () = c.set(own, value, None, None, false).await.unwrap();
                    1
                }
                "mget" => {
                    let v: Vec<Vec<u8>> = c.mget(keys.to_vec()).await.unwrap();
                    v.len()
                }
                w => panic!("unknown workload {w}"),
            },
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let which = args[1].clone();
    let port: u16 = args[2].parse().unwrap();
    let workload = args[3].clone();
    let tasks: usize = args[4].parse().unwrap();
    let secs: u64 = args[5].parse().unwrap();
    let size: usize = std::env::var("POOL_SIZE").map_or(1, |v| v.parse().unwrap());
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let mut conns = Vec::new();
        for _ in 0..size {
            conns.push(match which.as_str() {
                "redis-rs" => {
                    let client =
                        redis::Client::open(format!("redis://127.0.0.1:{port}/")).unwrap();
                    Conn::RedisRs(client.get_multiplexed_async_connection().await.unwrap())
                }
                "fred" => {
                    let config = fred::types::config::Config::from_url(&format!(
                        "redis://127.0.0.1:{port}"
                    ))
                    .unwrap();
                    let client = fred::clients::Client::new(config, None, None, None);
                    client.init().await.unwrap();
                    Conn::Fred(client)
                }
                w => panic!("unknown client {w}"),
            });
        }
        let mut setup = conns[0].clone();
        let keys: Vec<String> = (0..10).map(|i| format!("bench:m{i}")).collect();
        let v64 = vec![b'v'; 64];
        setup.run("set", "bench:64", &v64, &keys).await;
        setup.run("set", "bench:8k", &vec![b'v'; 8192], &keys).await;
        for k in &keys {
            setup.run("set", k, &v64, &keys).await;
        }
        let keys = Rc::new(keys);
        let measuring = Rc::new(Cell::new(false));
        let stop = Rc::new(Cell::new(false));
        let samples = Rc::new(RefCell::new(Vec::<u32>::with_capacity(1 << 24)));
        let sink = Rc::new(Cell::new(0usize));
        let handles: Vec<_> = (0..tasks)
            .map(|t| {
                let (mut conn, workload, measuring, stop, samples, sink, keys) = (
                    conns[t % size].clone(),
                    workload.clone(),
                    measuring.clone(),
                    stop.clone(),
                    samples.clone(),
                    sink.clone(),
                    keys.clone(),
                );
                tokio::task::spawn_local(async move {
                    let own = format!("bench:set:{t}");
                    let value = vec![b's'; 64];
                    while !stop.get() {
                        let start = Instant::now();
                        let n = conn.run(&workload, &own, &value, &keys).await;
                        sink.set(sink.get() + n);
                        if measuring.get() {
                            samples.borrow_mut().push(start.elapsed().as_micros() as u32);
                        }
                    }
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_secs(2)).await;
        measuring.set(true);
        let ticks = cpu_ticks();
        let begin = Instant::now();
        tokio::time::sleep(Duration::from_secs(secs)).await;
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
            "ops={:.0} p50_us={} p99_us={} cpu={:.0} sink={}",
            s.len() as f64 / elapsed,
            pct(0.50),
            pct(0.99),
            cpu,
            sink.get() % 10
        );
    });
}
