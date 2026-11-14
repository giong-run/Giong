//! Load generator for bench/http-client.sh: giong-http's client on one giong
//! worker.
//!
//!   bench-http-client-giong <url> <tasks> <seconds>
//!
//! One `Client` shared by every task, with at most POOL_SIZE connections to
//! the server (16 by default, the client's own default). With CA set to a
//! PEM file, that authority is trusted for `https` URLs, and GIONG_KTLS=auto
//! or required hands the connections to kernel TLS after the handshake.
//! HTTP=1 speaks HTTP/1.1 only; HTTP=h2c speaks HTTP/2 to `http` URLs (prior
//! knowledge); otherwise HTTP/2 is offered through ALPN to `https` URLs.
//! Prints `ops=<per second> p50_us=<..> p99_us=<..> cpu=<%> ktls=<connections>
//! version=<..>`
//! for the measured part, after a warm-up of 2 seconds.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong_http::client::Client;
use giong_tls::{Ktls, TlsConnector};

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
    let size: usize = std::env::var("POOL_SIZE").map_or(16, |v| v.parse().unwrap());

    let mut builder = Client::builder().max_connections_per_host(size);
    if let Ok(ca) = std::env::var("CA") {
        let connector = TlsConnector::with_root_pem(&std::fs::read(ca).unwrap()).unwrap();
        let mode = match std::env::var("GIONG_KTLS").as_deref() {
            Ok("auto") => Ktls::Auto,
            Ok("required") => Ktls::Required,
            _ => Ktls::Off,
        };
        builder = builder.tls(connector.ktls(mode).unwrap());
    }
    match std::env::var("HTTP").as_deref() {
        Ok("1") => builder = builder.http2(false),
        Ok("h2c") => builder = builder.http2_prior_knowledge(true),
        _ => {}
    }
    let client = builder.build();

    giong::block_on(async move {
        let version = client.get(&url).send().await.unwrap().version().as_str();
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
                giong::spawn(async move {
                    while !stop.get() {
                        let start = Instant::now();
                        let response = client.get(&url).send().await.unwrap();
                        assert_eq!(response.status(), 200);
                        sink.set(sink.get() + response.body().len());
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
            "ops={:.0} p50_us={} p99_us={} cpu={:.0} sink={} ktls={} version={version}",
            s.len() as f64 / elapsed,
            pct(0.50),
            pct(0.99),
            cpu,
            sink.get() % 10,
            giong_tls::ktls_stats().offloaded
        );
    });
}
