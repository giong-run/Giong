//! Balanced listeners choosing by how busy the workers have been (CPU
//! time), not only by counts of tasks. In a test binary of their own, so
//! that other tests do not take the CPU time the busy worker here needs to
//! look busy.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod common;

#[test]
fn a_saturated_worker_with_few_tasks_loses_new_connections_to_a_quiet_one_with_many() {
    // Worker 0 holds 50 idle tasks (think idle keep-alive connections);
    // worker 1 holds one task that keeps it busy all the time (think a few
    // hot connections). By counts of tasks worker 1 looks far less busy; by
    // how busy the workers have been, worker 0 is the one with room. Every
    // new connection must go to worker 0, whichever listener the kernel
    // picks for it.
    assert_eq!(
        saturated_worker_case(None),
        vec![8, 0],
        "worker 1 is saturated"
    );
}

#[test]
fn time_spent_spinning_for_work_does_not_count_as_busy() {
    // The same, with every worker spinning before it sleeps, for longer
    // than the test runs: worker 0 is awake all the time but has nothing to
    // do, so it still has room.
    let spin = Some(Duration::from_secs(10));
    assert_eq!(
        saturated_worker_case(spin),
        vec![8, 0],
        "worker 1 is saturated"
    );
}

/// Two workers with balanced listeners: worker 0 holds 50 idle tasks,
/// worker 1 one task that never lets it rest. A plain thread opens 8
/// connections a few milliseconds apart; returns how many each worker got.
fn saturated_worker_case(spin: Option<Duration>) -> Vec<usize> {
    let (_holder, port) = common::shared_port();
    let mut builder = giong::Builder::new().workers(2);
    if let Some(spin) = spin {
        builder = builder.spin_before_park(spin);
    }
    let rt = builder.build().unwrap();
    let handle = rt.handle();
    let counts = Arc::new(Mutex::new(vec![0usize; 2]));
    let ready = Arc::new(std::sync::Barrier::new(3));
    const CONNECTIONS: usize = 8;
    let (c, r) = (counts.clone(), ready.clone());
    let client_ready = ready.clone();
    let client = std::thread::spawn(move || {
        client_ready.wait();
        // Long enough for the busy worker's time to show in a sample.
        std::thread::sleep(Duration::from_millis(30));
        let streams: Vec<_> = (0..CONNECTIONS)
            .map(|_| {
                std::thread::sleep(Duration::from_millis(7));
                std::net::TcpStream::connect(("127.0.0.1", port)).unwrap()
            })
            .collect();
        streams
    });
    rt.run(move |index| {
        let (counts, ready, handle) = (c.clone(), r.clone(), handle.clone());
        async move {
            if index == 0 {
                for _ in 0..50 {
                    drop(giong::spawn(giong::shutdown_signal()));
                }
            } else {
                drop(giong::spawn(async {
                    while !giong::is_shutting_down() {
                        let start = Instant::now();
                        while start.elapsed() < Duration::from_micros(500) {
                            std::hint::spin_loop();
                        }
                        giong::yield_now().await;
                    }
                }));
            }
            giong::yield_now().await;
            let listener = giong::net::TcpListener::bind_balanced(("127.0.0.1", port)).unwrap();
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
                    if total == CONNECTIONS {
                        handle.shutdown();
                    }
                }
            };
            giong::until_shutdown(accept).await;
        }
    })
    .unwrap();
    drop(client.join().unwrap());
    let counts = counts.lock().unwrap().clone();
    counts
}
