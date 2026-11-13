//! Choosing where new work starts: `Handle::spawn_balanced` and
//! `Handle::least_loaded_worker`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giong::time::sleep;

#[test]
fn a_burst_of_balanced_jobs_spreads_over_every_worker() {
    const WORKERS: usize = 4;
    const JOBS: usize = 8;
    let rt = giong::Builder::new().workers(WORKERS).build().unwrap();
    let handle = rt.handle();
    let ran_on = Arc::new(Mutex::new(Vec::new()));
    let r = ran_on.clone();
    rt.run(move |index| {
        let handle = handle.clone();
        let ran_on = r.clone();
        async move {
            if index == 0 {
                // Jobs that hold their worker's thread, as CPU-heavy work does.
                let jobs: Vec<_> = (0..JOBS)
                    .map(|_| {
                        handle.spawn_balanced(|| async {
                            std::thread::sleep(Duration::from_millis(20));
                            giong::worker_id()
                        })
                    })
                    .collect();
                for job in jobs {
                    let worker = job.await.unwrap();
                    ran_on.lock().unwrap().push(worker);
                }
                handle.shutdown();
            }
            giong::shutdown_signal().await;
        }
    })
    .unwrap();

    let ran_on = ran_on.lock().unwrap();
    assert_eq!(ran_on.len(), JOBS);
    let mut per_worker: HashMap<usize, usize> = HashMap::new();
    for &w in ran_on.iter() {
        *per_worker.entry(w).or_default() += 1;
    }
    assert_eq!(
        per_worker.len(),
        WORKERS,
        "every worker gets work: {per_worker:?}"
    );
    assert!(
        per_worker.values().all(|&n| n <= 3),
        "no worker gets most of it: {per_worker:?}"
    );
}

#[test]
fn a_worker_stuck_in_a_long_task_is_not_chosen() {
    const WORKERS: usize = 4;
    let rt = giong::Builder::new().workers(WORKERS).build().unwrap();
    let handle = rt.handle();
    let busy = Arc::new(AtomicBool::new(false));
    let chosen = Arc::new(Mutex::new(Vec::new()));
    let (b, c) = (busy.clone(), chosen.clone());
    rt.run(move |index| {
        let (handle, busy, chosen) = (handle.clone(), b.clone(), c.clone());
        async move {
            match index {
                1 => {
                    busy.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(300));
                    busy.store(false, Ordering::SeqCst);
                }
                0 => {
                    let started = Instant::now();
                    while !busy.load(Ordering::SeqCst) {
                        assert!(started.elapsed() < Duration::from_secs(10));
                        sleep(Duration::from_millis(1)).await;
                    }
                    for _ in 0..10 {
                        let w = handle.least_loaded_worker();
                        chosen.lock().unwrap().push(w);
                    }
                    while busy.load(Ordering::SeqCst) {
                        sleep(Duration::from_millis(5)).await;
                    }
                    handle.shutdown();
                }
                _ => {}
            }
            giong::shutdown_signal().await;
        }
    })
    .unwrap();

    let chosen = chosen.lock().unwrap();
    assert_eq!(chosen.len(), 10);
    assert!(
        !chosen.contains(&1),
        "the stuck worker was chosen: {chosen:?}"
    );
}

#[test]
fn least_loaded_worker_works_from_any_thread_and_spreads_ties() {
    const WORKERS: usize = 4;
    let rt = giong::Builder::new().workers(WORKERS).build().unwrap();
    let handle = rt.handle();
    // Before the workers start every one looks the same: the picks rotate.
    let picks: HashSet<usize> = (0..WORKERS).map(|_| handle.least_loaded_worker()).collect();
    assert_eq!(picks, (0..WORKERS).collect());
    // Work sent to a worker counts at once, before it runs.
    drop(handle.spawn_on(2, || async {}));
    let picks: HashSet<usize> = (0..2 * WORKERS)
        .map(|_| handle.least_loaded_worker())
        .collect();
    assert_eq!(picks, HashSet::from([0, 1, 3]));
    let h = handle.clone();
    let from_thread = std::thread::spawn(move || h.least_loaded_worker())
        .join()
        .unwrap();
    assert!(from_thread < WORKERS);
}

#[test]
fn a_balanced_task_that_panics_reports_it_and_its_worker_carries_on() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    let results = Arc::new(Mutex::new(Vec::new()));
    let r = results.clone();
    rt.run(move |index| {
        let (handle, results) = (handle.clone(), r.clone());
        async move {
            if index == 0 {
                for round in 0..4 {
                    let failed = handle
                        .spawn_balanced(move || async move {
                            if round % 2 == 0 {
                                panic!("job {round} fails");
                            }
                            round
                        })
                        .await;
                    results.lock().unwrap().push(failed.is_err());
                }
                handle.shutdown();
            }
            giong::shutdown_signal().await;
        }
    })
    .unwrap();
    assert_eq!(*results.lock().unwrap(), vec![true, false, true, false]);
}

/// Runs `workers` workers that each accept on a listener made by `bind`
/// (all on one port), keep every connection open, and count them; worker 1
/// first spawns `busy` idle tasks so that it looks busier. A plain thread
/// opens `connections` connections. Returns the count per worker.
fn connections_per_worker(
    workers: usize,
    busy: usize,
    connections: usize,
    balanced: bool,
) -> Vec<usize> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let rt = giong::Builder::new().workers(workers).build().unwrap();
    let handle = rt.handle();
    let counts = Arc::new(Mutex::new(vec![0usize; workers]));
    let ready = Arc::new(std::sync::Barrier::new(workers + 1));
    let done = Arc::new(std::sync::Barrier::new(2));
    let (c, r, d) = (counts.clone(), ready.clone(), done.clone());
    let client = std::thread::spawn(move || {
        r.wait();
        let streams: Vec<_> = (0..connections)
            .map(|_| std::net::TcpStream::connect(("127.0.0.1", port)).unwrap())
            .collect();
        d.wait();
        drop(streams);
    });
    rt.run(move |index| {
        let (counts, ready, done, handle) =
            (c.clone(), ready.clone(), done.clone(), handle.clone());
        async move {
            if index == 1 {
                for _ in 0..busy {
                    drop(giong::spawn(giong::shutdown_signal()));
                }
                // Let the worker publish its task count.
                giong::yield_now().await;
            }
            let addr = ("127.0.0.1", port);
            let listener = if balanced {
                giong::net::TcpListener::bind_balanced(addr).unwrap()
            } else {
                giong::net::TcpListener::bind(addr).unwrap()
            };
            ready.wait();
            let accept = async {
                loop {
                    let stream = listener.accept().await.unwrap();
                    let total = {
                        let mut counts = counts.lock().unwrap();
                        counts[index] += 1;
                        counts.iter().sum::<usize>()
                    };
                    drop(giong::spawn(async move {
                        let _keep = stream;
                        giong::shutdown_signal().await;
                    }));
                    if total == connections {
                        let done = done.clone();
                        std::thread::spawn(move || done.wait());
                        handle.shutdown();
                    }
                }
            };
            giong::until_shutdown(accept).await;
        }
    })
    .unwrap();
    client.join().unwrap();
    let counts = counts.lock().unwrap().clone();
    counts
}

#[test]
fn a_busy_worker_hands_new_connections_to_an_idle_one() {
    let counts = connections_per_worker(2, 20, 8, true);
    assert_eq!(
        counts,
        vec![8, 0],
        "worker 1 looks busy, so worker 0 gets every connection"
    );
}

#[test]
fn balanced_listeners_keep_connection_counts_close() {
    // A worker decides from hints that can be a moment old, and cannot see
    // the connections the kernel has queued for another worker, so a run now
    // and then ends a few apart. Over 300 runs on 4 vCPUs the spread
    // (busiest minus quietest) was 3 or less in 97% of them and 6 at worst;
    // plain SO_REUSEPORT listeners had a median of 5 and reached 12.
    //
    // ThreadSanitizer slows the threads down unevenly, which makes the
    // hints staler: there the test still runs every hand-over path, but
    // does not judge the spread.
    let judge = !cfg!(giong_tsan);
    const RUNS: usize = 9;
    let mut spreads: Vec<usize> = (0..RUNS)
        .map(|_| {
            let counts = connections_per_worker(4, 0, 24, true);
            assert_eq!(counts.iter().sum::<usize>(), 24);
            let spread = counts.iter().max().unwrap() - counts.iter().min().unwrap();
            assert!(!judge || spread <= 8, "{counts:?}");
            spread
        })
        .collect();
    spreads.sort_unstable();
    assert!(
        !judge || spreads[RUNS / 2] <= 3,
        "median spread too wide: {spreads:?}"
    );
}

#[test]
#[ignore = "prints the kernel's own spread, for comparison"]
fn print_reuseport_spread() {
    for _ in 0..10 {
        println!(
            "plain {:?}  balanced {:?}",
            connections_per_worker(4, 0, 24, false),
            connections_per_worker(4, 0, 24, true)
        );
    }
}
