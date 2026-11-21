//! The blocking and compute lanes: `spawn_blocking`, `spawn_compute`,
//! `try_spawn_compute` and their `Handle` forms.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use giong::error::{self, Code};
use giong::time::sleep;

fn thread_name() -> String {
    std::thread::current().name().unwrap_or("").to_owned()
}

fn nice() -> i32 {
    // SAFETY: plain syscall; `who == 0` is the calling thread on Linux.
    unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
}

#[test]
fn a_lane_handle_can_be_sent_to_other_threads() {
    fn assert_send_sync<T: Send + Sync + Unpin>() {}
    assert_send_sync::<giong::BlockingHandle<Vec<u8>>>();
}

#[test]
fn a_blocking_closure_runs_on_a_lane_thread_and_gives_its_value() {
    giong::block_on(async {
        let worker = std::thread::current().id();
        let (name, id, prio) =
            giong::spawn_blocking(|| (thread_name(), std::thread::current().id(), nice()))
                .await
                .unwrap();
        assert_eq!(name, "giong-lane-block");
        assert_ne!(id, worker);
        assert_eq!(prio, nice(), "blocking threads keep the workers' priority");
    });
}

#[test]
fn a_compute_closure_runs_below_the_workers_priority() {
    giong::block_on(async {
        let (name, prio) = giong::spawn_compute(|| (thread_name(), nice()))
            .await
            .unwrap();
        assert_eq!(name, "giong-lane-cpu");
        assert_eq!(prio, nice() + 10);
    });
}

#[test]
fn a_blocked_closure_does_not_stall_the_worker() {
    giong::block_on(async {
        let job = giong::spawn_blocking(|| std::thread::sleep(Duration::from_millis(300)));
        // The worker keeps running timers while the closure sleeps.
        let start = Instant::now();
        let mut ticks = 0;
        while !job.is_finished() {
            sleep(Duration::from_millis(10)).await;
            ticks += 1;
        }
        job.await.unwrap();
        assert!(ticks >= 10, "only {ticks} ticks in {:?}", start.elapsed());
    });
}

#[test]
fn a_panic_in_a_closure_reaches_the_handle_not_the_worker() {
    giong::block_on(async {
        let err = giong::spawn_blocking(|| -> u8 { panic!("boom") })
            .await
            .unwrap_err();
        assert!(err.is_panic());
        let payload = err.try_into_panic().ok().unwrap();
        assert_eq!(payload.downcast_ref::<&str>(), Some(&"boom"));
        let err = giong::spawn_compute(|| -> u8 { panic!("boom") })
            .await
            .unwrap_err();
        assert!(err.is_panic());
        // The lane thread survived and takes the next closure.
        assert_eq!(giong::spawn_compute(|| 7).await.unwrap(), 7);
    });
}

#[test]
fn abort_stops_a_closure_that_has_not_started() {
    giong::Builder::new()
        .compute_threads(1)
        .block_on(async {
            let (release, wait) = mpsc::channel::<()>();
            let busy = giong::spawn_compute(move || wait.recv().unwrap());
            let ran = Arc::new(AtomicBool::new(false));
            let r = ran.clone();
            let queued = giong::spawn_compute(move || r.store(true, Ordering::SeqCst));
            assert!(queued.abort());
            assert!(queued.is_finished());
            assert!(queued.await.unwrap_err().is_cancelled());
            release.send(()).unwrap();
            busy.await.unwrap();
            // A closure that ran cannot be aborted any more.
            let done = giong::spawn_compute(|| 1);
            while !done.is_finished() {
                giong::yield_now().await;
            }
            assert!(!done.abort());
            assert_eq!(done.await.unwrap(), 1);
            assert!(!ran.load(Ordering::SeqCst));
        })
        .unwrap();
}

#[test]
fn try_spawn_compute_sheds_once_the_backlog_is_full() {
    giong::Builder::new()
        .compute_threads(1)
        .compute_backlog(2)
        .block_on(async {
            let gate = Arc::new(Barrier::new(2));
            let g = gate.clone();
            let busy = giong::spawn_compute(move || {
                g.wait();
            });
            // Wait until the thread holds the first closure, so the rest queue.
            while giong::handle().lane_stats().compute.queued != 0 {
                giong::yield_now().await;
            }
            let a = giong::try_spawn_compute(|| 1).unwrap();
            let b = giong::try_spawn_compute(|| 2).unwrap();
            let err = giong::try_spawn_compute(|| 3).unwrap_err();
            assert_eq!(error::code(&err), Some(Code::ComputeBusy));
            assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
            // spawn_compute itself never refuses.
            let c = giong::spawn_compute(|| 4);
            gate.wait();
            busy.await.unwrap();
            assert_eq!(a.await.unwrap() + b.await.unwrap() + c.await.unwrap(), 7);
        })
        .unwrap();
}

#[test]
fn the_blocking_lane_never_has_more_threads_than_its_limit() {
    giong::Builder::new()
        .max_blocking_threads(2)
        .block_on(async {
            let running = Arc::new(AtomicUsize::new(0));
            let peak = Arc::new(AtomicUsize::new(0));
            let jobs: Vec<_> = (0..8)
                .map(|i| {
                    let running = running.clone();
                    let peak = peak.clone();
                    giong::spawn_blocking(move || {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(50));
                        running.fetch_sub(1, Ordering::SeqCst);
                        i
                    })
                })
                .collect();
            let stats = giong::handle().lane_stats().blocking;
            assert!(stats.threads <= 2, "{stats:?}");
            assert_eq!(stats.max_threads, 2);
            let mut sum = 0;
            for job in jobs {
                sum += job.await.unwrap();
            }
            assert_eq!(sum, 28);
            assert_eq!(peak.load(Ordering::SeqCst), 2);
            assert_eq!(giong::handle().lane_stats().blocking.completed, 8);
        })
        .unwrap();
}

#[test]
fn the_handle_spawns_from_a_plain_thread() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let h = handle.clone();
    let from_outside = std::thread::spawn(move || (h.spawn_blocking(|| 40), h.spawn_compute(|| 2)))
        .join()
        .unwrap();
    let from_outside = std::sync::Mutex::new(Some(from_outside));
    rt.run(move |_| {
        let (a, b) = from_outside.lock().unwrap().take().unwrap();
        let handle = handle.clone();
        async move {
            assert_eq!(a.await.unwrap() + b.await.unwrap(), 42);
            handle.shutdown();
        }
    })
    .unwrap();
}

#[test]
fn closures_that_have_not_started_are_cancelled_when_the_runtime_stops() {
    let rt = giong::Builder::new().compute_threads(1).build().unwrap();
    let handle = rt.handle();
    let (release, wait) = mpsc::channel::<()>();
    let (started_tx, started) = mpsc::channel::<()>();
    let busy = handle.spawn_compute(move || {
        started_tx.send(()).unwrap();
        wait.recv().unwrap();
    });
    started.recv().unwrap();
    let queued = handle.spawn_compute(|| 1);
    drop(rt);
    // Work sent after the runtime is gone is cancelled at once too.
    let late = handle.spawn_blocking(|| 2);
    release.send(()).unwrap();
    giong::block_on(async move {
        assert!(queued.await.unwrap_err().is_cancelled());
        assert!(late.await.unwrap_err().is_cancelled());
        // The closure that had started ran to its end.
        busy.await.unwrap();
    });
}

#[test]
#[should_panic(expected = "GIONG-E0011")]
fn spawn_blocking_outside_a_runtime_panics_with_a_code() {
    drop(giong::spawn_blocking(|| ()));
}
