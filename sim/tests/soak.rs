//! Long runs in simulated time: weeks of uptime for an HTTP server and its
//! client, with an hourly burst of requests and quiet hours between them,
//! plus timers that span days. Real time per simulated week is a few
//! seconds, because quiet hours cost nothing.
//!
//! What it checks:
//! - every request gets its own answer or an error (benign faults: never an
//!   error);
//! - nothing grows per request: after each quiet spell the open
//!   descriptors, tasks, timers and tracked operations are what they were
//!   at startup, plus at most the client's pool (it keeps idle connections,
//!   closed by the server, until its next request sweeps them), and once
//!   the client is dropped only the server's descriptors are left;
//! - a sleep for half the run wakes on time, and 10-minute ticks do not
//!   drift.
//!
//! Under hostile faults the clock may jump forward by up to 30 s. A jump
//! while a request is on its way can use up the server's request timeout,
//! and then the server's 408 is the right answer, so it counts as an error,
//! but only when the request really took that long on the client's clock.
//!
//! `GIONG_SIM_SOAK_DAYS` sets the length (default 90 days; the hostile runs
//! take a seventh of that each, over `GIONG_SIM_SEEDS` seeds, default 10,
//! starting at seed `GIONG_SIM_SEED_FROM`, default 0).

use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong_http::client::Client;
use giong_http::{Request, Response, Server};
use giong_sim::{Faults, Sim};

const BURST: usize = 12;
const POOL: usize = 4;
const HOUR: Duration = Duration::from_secs(3600);
/// The server's request timeout: the most a request may take to arrive.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

async fn echo(req: Request) -> Response {
    let mut body = req.path().as_bytes().to_vec();
    body.push(b'|');
    body.extend_from_slice(req.body());
    Response::new(200).body(body)
}

/// What is alive in the runtime and the simulated kernel.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Alive {
    descriptors: usize,
    tasks: usize,
    timers: usize,
    tracked_ops: usize,
    inflight: usize,
}

fn alive() -> Alive {
    let s = giong::observe::worker_stats();
    Alive {
        descriptors: giong_sim::open_descriptors(),
        tasks: s.tasks,
        timers: s.timers,
        tracked_ops: s.tracked_ops,
        inflight: s.inflight,
    }
}

#[derive(Debug, Default)]
struct Outcome {
    ok: usize,
    errors: usize,
    hours: usize,
}

/// `jumps`: the monotonic clock may jump forward (a stalled host), so
/// timers can be late by as much as one jump (30 s), never early.
fn soak(days: u64, jumps: bool) -> Outcome {
    giong::block_on(async move {
        let listener = TcpListener::bind("127.0.0.1:8080").unwrap();
        // The low-memory refusal reads the host's free memory, which must
        // not decide a simulated run.
        let server = Server::new()
            .min_free_memory(0)
            .idle_timeout(Duration::from_secs(60))
            .request_timeout(REQUEST_TIMEOUT);
        giong::spawn(async move { server.serve(listener, echo).await });
        let client = Rc::new(
            Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(10))
                .idle_timeout(Duration::from_secs(90))
                .max_connections_per_host(POOL)
                .build(),
        );

        // Timers that span the run: a sleep for half of it, and a
        // 10-minute ticker.
        let start = Instant::now();
        let half = Duration::from_secs(days * 43_200);
        let long = giong::spawn(async move {
            giong::time::sleep(half).await;
            start.elapsed()
        });
        let ticks = giong::spawn(async move {
            let mut n = 0u64;
            let mut next = start;
            loop {
                next += Duration::from_secs(600);
                giong::time::sleep_until(next).await;
                n += 1;
                if next.duration_since(start) >= Duration::from_secs(days * 86_400) {
                    return (n, Instant::now().duration_since(next));
                }
            }
        });

        // Once the server waits in accept.
        giong::time::sleep(Duration::from_millis(10)).await;
        let startup = alive();
        let mut out = Outcome::default();
        for hour in 0..days * 24 {
            let tasks: Vec<_> = (0..BURST)
                .map(|t| {
                    let client = client.clone();
                    giong::spawn(async move {
                        let path = format!("/h{hour}/t{t}");
                        let body = vec![b'a' + (t % 26) as u8; 50 + t * 211];
                        let sent = Instant::now();
                        match client
                            .post(&format!("http://127.0.0.1:8080{path}"))
                            .body(body.clone())
                            .send()
                            .await
                        {
                            // The request timed out on the server: right
                            // only if a clock jump made it take that long.
                            Ok(resp) if resp.status() == 408 => {
                                let took = sent.elapsed();
                                assert!(
                                    jumps && took >= REQUEST_TIMEOUT,
                                    "408 for {path} after only {took:?}"
                                );
                                false
                            }
                            Ok(resp) => {
                                assert_eq!(resp.status(), 200);
                                let mut want = path.into_bytes();
                                want.push(b'|');
                                want.extend_from_slice(&body);
                                assert!(
                                    resp.body() == want,
                                    "a response went to the wrong request"
                                );
                                true
                            }
                            // Benign faults never show as errors: fail at
                            // once, so that the trace shows how it came.
                            Err(e) => {
                                assert!(jumps, "{path} at {:?}: {e}", start.elapsed());
                                false
                            }
                        }
                    })
                })
                .collect();
            for t in tasks {
                if t.await.unwrap() {
                    out.ok += 1;
                } else {
                    out.errors += 1;
                }
            }
            // A quiet spell longer than both idle timeouts: every
            // connection must be gone afterwards.
            giong::time::sleep(Duration::from_secs(5 * 60)).await;
            // The client keeps up to POOL connections, closed by the
            // server, until its next request sweeps them; beyond that
            // nothing may stay behind.
            let now = alive();
            assert!(
                now.descriptors <= startup.descriptors + POOL
                    && now.tracked_ops <= startup.tracked_ops + POOL
                    && now.inflight == 0
                    && now.tasks <= startup.tasks
                    && now.timers <= startup.timers,
                "hour {hour}: something grows or stays behind: {now:?}, at startup {startup:?}"
            );
            out.hours += 1;
            let next = start + HOUR * (hour as u32 + 1);
            giong::time::sleep_until(next).await;
        }

        // Without the client, everything but the server is gone.
        drop(client);
        giong::time::sleep(Duration::from_secs(5 * 60)).await;
        let end = alive();
        assert_eq!(
            end.descriptors, startup.descriptors,
            "at the end: {end:?}, at startup {startup:?}"
        );
        assert_eq!(
            end.tracked_ops, startup.tracked_ops,
            "at the end: {end:?}, at startup {startup:?}"
        );

        let slept = long.await.unwrap();
        let slack = Duration::from_secs(if jumps { 31 } else { 1 });
        assert!(
            slept >= half && slept < half + slack,
            "a sleep of {half:?} took {slept:?}"
        );
        let (n, late) = ticks.await.unwrap();
        assert_eq!(n, days * 24 * 6, "10-minute ticks");
        assert!(late < slack, "the last tick was {late:?} late");
        out
    })
}

fn days() -> u64 {
    std::env::var("GIONG_SIM_SOAK_DAYS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90)
}

#[test]
fn weeks_of_uptime_under_benign_faults() {
    let days = days();
    let real = Instant::now();
    let (out, r) = Sim::new(giong_sim::replay_seed().unwrap_or(7))
        .faults(Faults::benign())
        .time_limit(Duration::from_secs((days + 1) * 86_400))
        .run(move || soak(days, false));
    assert_eq!(out.errors, 0, "{out:?}");
    assert_eq!(out.ok, days as usize * 24 * BURST);
    assert!(r.open.is_empty(), "left open: {:?}", r.open);
    eprintln!(
        "soak: {:.1} simulated days, {} requests, {} steps, {:?} real",
        r.virtual_time.as_secs_f64() / 86_400.0,
        out.ok,
        r.steps,
        real.elapsed()
    );
}

#[test]
fn days_of_uptime_under_hostile_faults() {
    let days = (days() / 7).max(1);
    let seeds: u64 = std::env::var("GIONG_SIM_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let real = Instant::now();
    let mut simulated = Duration::ZERO;
    let from: u64 = std::env::var("GIONG_SIM_SEED_FROM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let seeds: Vec<u64> = match giong_sim::replay_seed() {
        Some(s) => vec![s],
        None => (from..from + seeds).collect(),
    };
    for seed in seeds {
        let (out, r) = Sim::new(seed)
            .faults(Faults::hostile())
            .time_limit(Duration::from_secs((days + 1) * 86_400))
            .run(move || soak(days, true));
        assert_eq!(out.ok + out.errors, days as usize * 24 * BURST);
        assert!(r.open.is_empty(), "seed {seed}: left open: {:?}", r.open);
        simulated += r.virtual_time;
    }
    eprintln!(
        "soak: {:.1} simulated days under hostile faults, {:?} real",
        simulated.as_secs_f64() / 86_400.0,
        real.elapsed()
    );
}
