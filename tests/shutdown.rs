//! Graceful shutdown: `Builder::shutdown_grace` lets a worker's remaining
//! tasks finish after its main future is done.

use std::cell::Cell;
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong::time::sleep;

/// Sets its flag when dropped, to observe a task being torn down.
struct OnDrop(Rc<Cell<bool>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

fn with_grace(grace: Duration) -> giong::Builder {
    giong::Builder::new().shutdown_grace(grace)
}

#[test]
fn remaining_tasks_finish_within_the_grace_period() {
    let finished = Rc::new(Cell::new(false));
    let f = finished.clone();
    let start = Instant::now();
    with_grace(Duration::from_secs(10))
        .block_on(async move {
            giong::spawn(async move {
                sleep(Duration::from_millis(100)).await;
                f.set(true);
            });
            // The main future returns while the task is still asleep.
        })
        .unwrap();
    assert!(finished.get(), "the task was dropped instead of drained");
    // The worker stops as soon as the last task is done, not after the grace.
    assert!(start.elapsed() >= Duration::from_millis(100));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn without_a_grace_period_remaining_tasks_are_dropped_at_once() {
    let finished = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    let (f, d) = (finished.clone(), dropped.clone());
    let start = Instant::now();
    giong::block_on(async move {
        giong::spawn(async move {
            let _guard = OnDrop(d);
            sleep(Duration::from_millis(100)).await;
            f.set(true);
        });
        giong::yield_now().await;
    });
    assert!(!finished.get());
    assert!(dropped.get());
    assert!(start.elapsed() < Duration::from_millis(100));
}

#[test]
fn tasks_that_outlive_the_grace_period_are_dropped() {
    let finished = Rc::new(Cell::new(false));
    let dropped = Rc::new(Cell::new(false));
    let (f, d) = (finished.clone(), dropped.clone());
    let start = Instant::now();
    with_grace(Duration::from_millis(150))
        .block_on(async move {
            giong::spawn(async move {
                let _guard = OnDrop(d);
                sleep(Duration::from_secs(60)).await;
                f.set(true);
            });
            giong::yield_now().await;
        })
        .unwrap();
    assert!(!finished.get());
    assert!(dropped.get(), "the task must still be dropped at the end");
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn a_worker_with_no_tasks_left_does_not_wait() {
    let start = Instant::now();
    with_grace(Duration::from_secs(30))
        .block_on(async {
            let task = giong::spawn(async { 7 });
            assert_eq!(task.await.unwrap(), 7);
        })
        .unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn draining_resolves_the_shutdown_signal() {
    // No shutdown is ever requested: the main future simply returns. Tasks
    // parked on `shutdown_signal` / `until_shutdown` must still be released.
    let released = Rc::new(Cell::new(0));
    let r = released.clone();
    let start = Instant::now();
    with_grace(Duration::from_secs(30))
        .block_on(async move {
            let r1 = r.clone();
            assert!(!giong::is_shutting_down());
            giong::spawn(async move {
                giong::shutdown_signal().await;
                assert!(giong::is_shutting_down());
                r1.set(r1.get() + 1);
            });
            giong::spawn(async move {
                let slept = giong::until_shutdown(sleep(Duration::from_secs(60))).await;
                assert!(slept.is_none());
                r.set(r.get() + 1);
            });
            // Let both tasks park first.
            sleep(Duration::from_millis(20)).await;
        })
        .unwrap();
    assert_eq!(released.get(), 2);
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn tasks_spawned_while_draining_also_run() {
    let finished = Rc::new(Cell::new(false));
    let f = finished.clone();
    with_grace(Duration::from_secs(10))
        .block_on(async move {
            giong::spawn(async move {
                sleep(Duration::from_millis(20)).await;
                giong::spawn(async move {
                    sleep(Duration::from_millis(20)).await;
                    f.set(true);
                });
            });
        })
        .unwrap();
    assert!(finished.get());
}

#[test]
fn a_panicking_task_does_not_break_the_drain() {
    let finished = Rc::new(Cell::new(false));
    let f = finished.clone();
    with_grace(Duration::from_secs(10))
        .block_on(async move {
            giong::spawn(async {
                sleep(Duration::from_millis(10)).await;
                panic!("boom (expected by this test)");
            });
            giong::spawn(async move {
                sleep(Duration::from_millis(50)).await;
                f.set(true);
            });
        })
        .unwrap();
    assert!(finished.get());
}

#[test]
fn the_signal_watcher_does_not_keep_a_draining_worker_alive() {
    let rt = giong::Builder::new()
        .workers(2)
        .shutdown_on_signals(true)
        .shutdown_grace(Duration::from_secs(30))
        .build()
        .unwrap();
    let handle = rt.handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        handle.shutdown();
    });
    let start = Instant::now();
    rt.run(|_| async {
        giong::shutdown_signal().await;
    })
    .unwrap();
    stopper.join().unwrap();
    assert!(start.elapsed() < Duration::from_secs(5));
}

/// The scenario the feature exists for: a server is told to stop while one
/// request is being worked on and another connection is idle.
#[test]
fn in_flight_requests_complete_and_idle_connections_close() {
    const REPLY: &[u8] = b"done";
    let rt = giong::Builder::new()
        .workers(2)
        .shutdown_grace(Duration::from_secs(30))
        .build()
        .unwrap();
    let handle = rt.handle();
    let (addr_tx, addr_rx) = mpsc::channel();
    let working = Arc::new(AtomicBool::new(false));
    let served = Arc::new(AtomicUsize::new(0));

    let (w, s) = (working.clone(), served.clone());
    let server = std::thread::spawn(move || {
        let start = Instant::now();
        rt.run(move |index| {
            let (addr_tx, w, s) = (addr_tx.clone(), w.clone(), s.clone());
            async move {
                // Only worker 0 listens, so both connections land on it.
                if index != 0 {
                    giong::shutdown_signal().await;
                    return;
                }
                let listener = TcpListener::bind("127.0.0.1:0").unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                while let Some(Ok(stream)) = giong::until_shutdown(listener.accept()).await {
                    let (w, s) = (w.clone(), s.clone());
                    giong::spawn(async move {
                        while let Some(Ok(data)) = giong::until_shutdown(stream.recv()).await {
                            if data.is_empty() {
                                return;
                            }
                            drop(data);
                            // A slow request: the shutdown arrives in here.
                            w.store(true, Ordering::SeqCst);
                            sleep(Duration::from_millis(300)).await;
                            if stream.write_all(REPLY.to_vec()).await.0.is_err() {
                                return;
                            }
                            s.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                }
            }
        })
        .unwrap();
        start.elapsed()
    });

    let addr = addr_rx.recv().unwrap();
    let mut idle = std::net::TcpStream::connect(addr).unwrap();
    let mut busy = std::net::TcpStream::connect(addr).unwrap();
    for conn in [&idle, &busy] {
        conn.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
    }
    busy.write_all(b"request").unwrap();
    while !working.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    handle.shutdown();

    // The request that was in flight is answered in full...
    let mut reply = [0u8; 4];
    busy.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, REPLY);
    // ...then its connection is closed rather than kept alive,
    assert_eq!(busy.read(&mut reply).unwrap(), 0);
    // and the idle connection is closed without waiting for the grace period.
    assert_eq!(idle.read(&mut reply).unwrap(), 0);

    let elapsed = server.join().unwrap();
    assert_eq!(served.load(Ordering::SeqCst), 1);
    assert!(
        elapsed < Duration::from_secs(10),
        "the runtime waited for the whole grace period: {elapsed:?}"
    );
}

/// `until_shutdown` adds a shutdown signal to the future it wraps, and no
/// second copy of it.
#[test]
fn until_shutdown_is_as_large_as_its_future_plus_a_signal() {
    type Big = std::future::Ready<[u8; 4096]>;
    let size = std::mem::size_of::<giong::UntilShutdown<Big>>();
    let parts = std::mem::size_of::<Big>() + std::mem::size_of::<giong::ShutdownSignal>();
    assert!(size <= parts + 8, "{size} bytes for {parts} bytes of parts");
}
