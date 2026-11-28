//! Start `k` children that each yield once and return a number, then wait
//! for all of them, `rounds` times over, on one giong worker.
//!
//!   bench-scope <mode> <k> <rounds>
//!
//! Modes:
//!   spawn   `giong::spawn` for each child, then await each `JoinHandle`
//!   group   a `TaskGroup`, then `join_next` until it is empty
//!   scope   a `Scope` whose body spawns the children and awaits their
//!           handles
//!
//! Prints nanoseconds per child.

use std::time::Instant;

use giong::task::{Scope, TaskGroup};

async fn child(i: u64) -> u64 {
    giong::yield_now().await;
    i
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "spawn".into());
    let k: u64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(10);
    let rounds: u64 = args.next().map(|s| s.parse().unwrap()).unwrap_or(10_000);
    let ns = giong::block_on(async move {
        let start = Instant::now();
        let mut sum = 0u64;
        for _ in 0..rounds {
            match mode.as_str() {
                "spawn" => {
                    let handles: Vec<_> = (0..k).map(|i| giong::spawn(child(i))).collect();
                    for h in handles {
                        sum += h.await.unwrap();
                    }
                }
                "group" => {
                    let mut group = TaskGroup::new();
                    for i in 0..k {
                        group.spawn(child(i));
                    }
                    while let Some(r) = group.join_next().await {
                        sum += r.unwrap();
                    }
                }
                "scope" => {
                    let scope = Scope::new();
                    sum += scope
                        .run(async {
                            let handles: Vec<_> = (0..k)
                                .map(|i| scope.spawn(async move { Ok(child(i).await) }))
                                .collect();
                            let mut s = 0;
                            for h in handles {
                                s += h.await;
                            }
                            Ok::<_, std::io::Error>(s)
                        })
                        .await
                        .unwrap();
                }
                other => panic!("unknown mode {other}"),
            }
        }
        assert_eq!(sum, rounds * k * k.saturating_sub(1) / 2);
        start.elapsed().as_nanos() as f64 / (rounds * k) as f64
    });
    println!("{ns:.0}");
}
