//! The server of `bench/tenants.sh`: every request reserves memory and runs
//! a CPU-bound job on the compute lane, then answers.
//!
//!     tenants-server <addr> <mode>
//!
//! `mode` is `shared` (no budgets: one FIFO compute queue and no limits,
//! as a service without isolation runs), `global` (one `Budget` for both
//! tenants with twice the limits: bounded, but not per tenant) or
//! `isolated` (one `Budget` per tenant, from the `x-tenant` header). All
//! use the waiting forms, no refusal.
//! Settings from the environment: `WORK_US` (CPU time per request, default
//! 2000), `MEM_KIB` (memory per request, touched, default 256),
//! `TENANT_MEM_MIB` (memory limit per tenant, default 32), `TENANT_COMPUTE`
//! (compute closures per tenant, default 8).

use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::budget::{Budget, Limits};
use giong::net::TcpListener;
use giong_http::{Request, Response, Server};

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Burns `us` microseconds of CPU, touching `mem` along the way.
fn work(us: u64, mem: &mut [u8]) -> u64 {
    let start = Instant::now();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut i = 0usize;
    while start.elapsed() < Duration::from_micros(us) {
        for _ in 0..256 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
        }
        if !mem.is_empty() {
            mem[i % mem.len()] = x as u8;
            i = i.wrapping_add(4096);
        }
    }
    x
}

/// The memory a request holds from its start to its answer (a body, a
/// decoded document), every page touched so that it is resident.
fn request_memory(bytes: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bytes];
    for page in buf.chunks_mut(4096) {
        page[0] = 1;
    }
    buf
}

fn main() {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:8080".into());
    let mode = args.next().unwrap_or_else(|| "shared".into());
    let (budgeted, per_tenant) = match mode.as_str() {
        "isolated" => (true, true),
        "global" => (true, false),
        "shared" => (false, false),
        other => panic!("mode is shared, global or isolated, not {other}"),
    };
    let work_us = env("WORK_US", 2000) as u64;
    let mem = env("MEM_KIB", 256) << 10;
    let (mem_mib, compute) = (env("TENANT_MEM_MIB", 32), env("TENANT_COMPUTE", 8));
    let limits = Limits::new().memory(mem_mib << 20).compute(compute);
    let global = Limits::new().memory(2 * mem_mib << 20).compute(2 * compute);
    let rt = giong::Builder::new()
        .workers(1)
        .shutdown_on_signals(true)
        .build()
        .expect("runtime");
    eprintln!(
        "tenants-server {addr} {} work={work_us}us mem={}KiB",
        mode,
        mem >> 10
    );
    rt.run(move |_| {
        let addr = addr.clone();
        async move {
            eprintln!("driver: {}", giong::driver_kind());
            let everyone = Budget::new("everyone", global);
            let budgets: Rc<HashMap<&'static str, Budget>> = Rc::new(
                ["a", "b"]
                    .into_iter()
                    .map(|t| {
                        let b = if per_tenant {
                            Budget::new(t, limits)
                        } else {
                            everyone.clone()
                        };
                        (t, b)
                    })
                    .collect(),
            );
            let listener = TcpListener::bind(addr.as_str()).expect("bind");
            Server::new()
                .serve(listener, move |req: Request| {
                    let budgets = budgets.clone();
                    async move {
                        let tenant = match req.header_str("x-tenant") {
                            Some("a") => "a",
                            Some("b") => "b",
                            _ => return Response::new(400),
                        };
                        if budgeted {
                            let budget = &budgets[tenant];
                            let Ok(reservation) = budget.reserve(mem).await else {
                                return Response::new(500);
                            };
                            let mut buf = request_memory(mem);
                            let job = budget
                                .spawn_compute(move || work(work_us, &mut buf))
                                .await;
                            let x = job.await.expect("job");
                            drop(reservation);
                            Response::text(format!("{x}\n"))
                        } else {
                            let mut buf = request_memory(mem);
                            let job = giong::spawn_compute(move || work(work_us, &mut buf));
                            let x = job.await.expect("job");
                            Response::text(format!("{x}\n"))
                        }
                    }
                })
                .await;
        }
    })
    .expect("run");
}
