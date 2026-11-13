//! Jobs of uneven CPU cost, all started from worker 0: how long until every
//! one is done, by how they are placed.
//!
//!   bench-spawn-balanced <jobs> <rounds>
//!
//! * `local`: `giong::spawn` on worker 0 (what a handler does by default);
//! * `round-robin`: `Handle::spawn_on(i % workers)`;
//! * `balanced`: `Handle::spawn_balanced`.
//!
//! Job sizes come from a fixed pseudo-random sequence (fib(20) to fib(29)),
//! the same in every round and for every placement. Prints the median and
//! the range of the rounds' wall times, in milliseconds.
use std::hint::black_box;
use std::time::{Duration, Instant};

fn fib(n: u32) -> u64 {
    if n < 2 {
        n as u64
    } else {
        fib(n - 1) + fib(n - 2)
    }
}

fn sizes(jobs: usize) -> Vec<u32> {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    (0..jobs)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            20 + (x % 10) as u32
        })
        .collect()
}

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let jobs: usize = args.get(1).map_or(64, |a| a.parse().unwrap());
    let rounds: usize = args.get(2).map_or(9, |a| a.parse().unwrap());
    let rt = giong::Builder::new().build()?;
    let handle = rt.handle();
    let workers = handle.workers();
    rt.run(move |index| {
        let handle = handle.clone();
        async move {
            if index != 0 {
                giong::shutdown_signal().await;
                return;
            }
            let sizes = sizes(jobs);
            println!("{workers} workers, {jobs} jobs (fib 20 to 29), {rounds} rounds; ms: median (min-max)");
            for placement in ["local", "round-robin", "balanced"] {
                let mut times = Vec::new();
                for _ in 0..rounds {
                    let started = Instant::now();
                    match placement {
                        "local" => {
                            let tasks: Vec<_> = sizes
                                .iter()
                                .map(|&n| giong::spawn(async move { fib(black_box(n)) }))
                                .collect();
                            for t in tasks {
                                black_box(t.await.unwrap());
                            }
                        }
                        "round-robin" => {
                            let tasks: Vec<_> = sizes
                                .iter()
                                .enumerate()
                                .map(|(i, &n)| handle.spawn_on(i % workers, move || async move { fib(black_box(n)) }))
                                .collect();
                            for t in tasks {
                                black_box(t.await.unwrap());
                            }
                        }
                        _ => {
                            let tasks: Vec<_> = sizes
                                .iter()
                                .map(|&n| handle.spawn_balanced(move || async move { fib(black_box(n)) }))
                                .collect();
                            for t in tasks {
                                black_box(t.await.unwrap());
                            }
                        }
                    }
                    times.push(started.elapsed());
                    giong::time::sleep(Duration::from_millis(20)).await;
                }
                times.sort();
                let ms = |d: Duration| d.as_secs_f64() * 1e3;
                println!(
                    "{placement:<12} {:8.1} ({:.1}-{:.1})",
                    ms(times[times.len() / 2]),
                    ms(times[0]),
                    ms(times[times.len() - 1])
                );
            }
            handle.shutdown();
        }
    })
}
