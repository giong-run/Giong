//! Load generator for bench/http-client.sh: reqwest on a current-thread
//! Tokio runtime (one core, like one giong worker).
//!
//!   bench-http-client-tokio <url> <tasks> <seconds>
//!
//! One `reqwest::Client` shared by every task, with its default pool (no
//! limit on connections per host; it keeps up to `usize::MAX` idle). With
//! POOL_SIZE set, at most that many are kept idle. With CA set to a PEM
//! file, that authority is trusted for `https` URLs.
//! Prints `ops=<per second> p50_us=<..> p99_us=<..> cpu=<%>` for the measured
//! part, after a warm-up of 2 seconds.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

/// CPU time this process has used, in clock ticks (user + system).
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").unwrap();
    let fields: Vec<&str> = stat.rsplit(')').next().unwrap().split_whitespace().collect();
    fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let url = args[1].clone();
    let tasks: usize = args[2].parse().unwrap();
    let secs: u64 = args[3].parse().unwrap();

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, async move {
        let mut builder = reqwest::Client::builder();
        if let Ok(size) = std::env::var("POOL_SIZE") {
            builder = builder.pool_max_idle_per_host(size.parse().unwrap());
        }
        if let Ok(ca) = std::env::var("CA") {
            let pem = std::fs::read(ca).unwrap();
            builder = builder
                .tls_built_in_root_certs(false)
                .add_root_certificate(reqwest::Certificate::from_pem(&pem).unwrap());
        }
        let client = builder.build().unwrap();
        let measuring = Rc::new(Cell::new(false));
        let stop = Rc::new(Cell::new(false));
        let samples = Rc::new(RefCell::new(Vec::<u32>::with_capacity(1 << 22)));
        let sink = Rc::new(Cell::new(0usize));
        let handles: Vec<_> = (0..tasks)
            .map(|_| {
                let (client, url, measuring, stop, samples, sink) = (
                    client.clone(),
                    url.clone(),
                    measuring.clone(),
                    stop.clone(),
                    samples.clone(),
                    sink.clone(),
                );
                tokio::task::spawn_local(async move {
                    while !stop.get() {
                        let start = Instant::now();
                        let response = client.get(&url).send().await.unwrap();
                        assert_eq!(response.status(), 200);
                        let body = response.bytes().await.unwrap();
                        sink.set(sink.get() + body.len());
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
