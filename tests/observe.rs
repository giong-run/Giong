//! `giong::observe`: pulses read from other threads, snapshots, profiles,
//! sources, and tasks named by where they were spawned.

use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use giong::observe::{self, Source, WorkerState};

fn here(file: &str) -> bool {
    file.ends_with("tests/observe.rs")
}

/// The longest a test blocks a worker on purpose, so that a failed check
/// ends the test instead of hanging it.
const BLOCK_LIMIT: Duration = Duration::from_secs(60);

#[test]
fn tasks_are_named_by_the_line_that_spawned_them() {
    giong::block_on(async {
        let line = line!() + 2;
        let tasks: Vec<_> = (0..5)
            .map(|_| giong::spawn(giong::time::sleep(Duration::from_secs(60))))
            .collect();
        // Let them start, and arm their timers.
        giong::yield_now().await;
        let snap = observe::snapshot();
        // The five, and nothing else: the main future is not a task.
        assert_eq!(snap.tasks, 5);
        assert_eq!(snap.tasks_by_location.len(), 1);
        let (location, count) = snap.tasks_by_location[0];
        assert!(here(location.file()), "{location}");
        assert_eq!(location.line(), line);
        assert_eq!(count, 5);
        assert_eq!(snap.timers, 5);
        assert_eq!(snap.worker, 0);
        assert_eq!(snap.driver, giong::driver_kind());
        for t in tasks {
            t.abort();
        }
    });
}

#[test]
fn spawn_on_and_spawn_balanced_name_their_caller() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    rt.run(move |worker| {
        let handle = handle.clone();
        async move {
            if worker != 0 {
                giong::shutdown_signal().await;
                return;
            }
            let on_line = line!() + 1;
            let parked = handle.spawn_on(1, || giong::time::sleep(Duration::from_secs(60)));
            let snap = handle
                .spawn_on(1, || async { observe::snapshot() })
                .await
                .unwrap();
            let lines: Vec<u32> = snap
                .tasks_by_location
                .iter()
                .filter(|(l, _)| here(l.file()))
                .map(|(l, _)| l.line())
                .collect();
            assert!(lines.contains(&on_line), "{lines:?} lacks {on_line}");
            parked.abort();

            let balanced_line = line!() + 1;
            let job = handle.spawn_balanced(|| async { observe::snapshot() });
            let snap = job.await.unwrap();
            assert!(
                snap.tasks_by_location
                    .iter()
                    .any(|(l, _)| here(l.file()) && l.line() == balanced_line),
                "{:?}",
                snap.tasks_by_location
            );
            handle.shutdown();
        }
    })
    .unwrap();
}

#[test]
fn a_pulse_shows_the_task_a_blocked_worker_is_stuck_in() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    rt.run(move |worker| {
        let handle = handle.clone();
        async move {
            if worker != 0 {
                giong::shutdown_signal().await;
                return;
            }
            // Blocks the thread, the mistake the pulse must expose, until
            // the checks below are done rather than for a fixed time: under
            // Miri or on a loaded machine they can take longer than any.
            let entered = Arc::new(AtomicBool::new(false));
            let release = Arc::new(AtomicBool::new(false));
            let (in_task, released) = (entered.clone(), release.clone());
            let line = line!() + 1;
            let blocker = handle.spawn_on(1, move || async move {
                in_task.store(true, Ordering::Release);
                let start = Instant::now();
                while !released.load(Ordering::Acquire) && start.elapsed() < BLOCK_LIMIT {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            // Wait until worker 1 is in it, then check that nothing moves.
            // A worker publishes its counters with relaxed stores, which a
            // reader may see a little late, one counter later than another
            // (Miri does that). Seeing `entered` puts every store worker 1
            // made before it in front of this thread's reads, so the first
            // pulse below is the state the worker blocks in.
            let start = Instant::now();
            while !entered.load(Ordering::Acquire) {
                assert!(start.elapsed() < BLOCK_LIMIT, "{:?}", handle.pulse(1));
                giong::time::sleep(Duration::from_millis(5)).await;
            }
            let first = handle.pulse(1);
            assert!(
                first
                    .polling
                    .is_some_and(|l| here(l.file()) && l.line() == line),
                "{first:?}"
            );
            giong::time::sleep(Duration::from_millis(200)).await;
            let later = handle.pulse(1);
            assert_eq!(later.state, WorkerState::Running);
            assert_eq!(later.polls, first.polls);
            assert_eq!(later.ticks, first.ticks);
            let location = later.polling.expect("polling");
            assert_eq!(location.line(), line);
            release.store(true, Ordering::Release);
            blocker.await.unwrap();

            // Afterwards worker 1 goes back to sleep and polls nothing.
            let start = Instant::now();
            loop {
                let p = handle.pulse(1);
                if p.state == WorkerState::Parked {
                    assert_eq!(p.polling, None);
                    break;
                }
                assert!(start.elapsed() < Duration::from_secs(5), "{p:?}");
                giong::time::sleep(Duration::from_millis(5)).await;
            }
            handle.shutdown();
        }
    })
    .unwrap();
}

#[test]
#[cfg_attr(
    miri,
    ignore = "timing: under Miri a 5 ms sleep is often over before the worker parks"
)]
fn counters_go_up_and_idle_time_is_counted() {
    giong::block_on(async {
        let h = giong::handle();
        let before = h.pulse(0);
        for _ in 0..10 {
            giong::time::sleep(Duration::from_millis(5)).await;
        }
        let after = h.pulse(0);
        assert!(after.ticks > before.ticks);
        assert!(after.polls >= before.polls + 10);
        assert!(after.parks >= before.parks + 10);
        assert!(
            after.idle >= before.idle + Duration::from_millis(30),
            "{after:?}"
        );
        assert_eq!(after.state, WorkerState::Running, "it is running this code");
        assert!(here(after.polling.expect("the main future").file()));
        assert_eq!(after.driver, Some(giong::driver_kind()));
    });
}

/// A worker with nothing to do sleeps for as long as nothing happens. Its
/// idle time has to grow meanwhile, not only when it next wakes: a reader
/// that takes `1 - idle / window` for how busy a worker is would otherwise
/// see a worker asleep for the whole window as fully busy.
#[test]
#[cfg_attr(miri, ignore = "timing: compares idle time with the clock")]
fn a_sleep_in_progress_counts_as_idle() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let watcher = std::thread::spawn(move || {
        let started = Instant::now();
        while handle.pulse(0).state != WorkerState::Parked {
            assert!(started.elapsed() < BLOCK_LIMIT, "never parked");
            std::thread::sleep(Duration::from_millis(1));
        }
        // Nothing wakes the worker from here on: one sleep, for the whole
        // window and longer.
        let t = Instant::now();
        let before = handle.pulse(0);
        std::thread::sleep(Duration::from_millis(200));
        let after = handle.pulse(0);
        let window = t.elapsed();
        handle.shutdown();
        (before, after, window)
    });
    rt.run(|_| async { giong::shutdown_signal().await })
        .unwrap();
    let (before, after, window) = watcher.join().unwrap();
    let idle = after.idle - before.idle;
    assert!(idle >= Duration::from_millis(190), "{idle:?}");
    assert!(idle <= window, "{idle:?} in {window:?}");
}

/// Read from another thread while the worker goes to sleep and wakes a
/// thousand times a second, idle time never goes down, never passes the
/// time since the runtime was built, and ends at what the sleeps add up to.
#[test]
#[cfg_attr(miri, ignore = "timing: compares idle time with the clock")]
fn idle_time_only_goes_up_while_a_worker_sleeps_and_wakes() {
    let built = Instant::now();
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = std::thread::spawn({
        let (handle, stop) = (handle.clone(), stop.clone());
        move || {
            let mut last = Duration::ZERO;
            while !stop.load(Ordering::Relaxed) {
                let idle = handle.pulse(0).idle;
                assert!(idle >= last, "{idle:?} after {last:?}");
                assert!(idle <= built.elapsed(), "{idle:?}");
                last = idle;
            }
        }
    });
    rt.run(move |_| {
        let stop = stop.clone();
        async move {
            for _ in 0..200 {
                giong::time::sleep(Duration::from_millis(1)).await;
            }
            stop.store(true, Ordering::Relaxed);
        }
    })
    .unwrap();
    watcher.join().unwrap();
    let end = handle.pulse(0);
    // Each sleep parks the worker once, unless its millisecond has passed
    // before the worker gets to park: then the timer fires without a sleep
    // in the kernel. Under ThreadSanitizer on epoll that happened once in
    // 200 (CI, #146). That skip needs a slow worker, so it stays rare; a
    // worker that stopped parking, or a park counter that missed parks,
    // would still fall far below this bound.
    assert!(end.parks >= 180, "{end:?}");
    assert!(end.idle >= Duration::from_millis(100), "{end:?}");
    assert!(end.idle <= built.elapsed(), "{end:?}");
}

#[test]
fn a_finished_runtime_reads_as_stopped() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    assert_eq!(handle.pulse(0).state, WorkerState::Stopped);
    rt.run(|_| async {}).unwrap();
    let p = handle.pulse(0);
    assert_eq!(p.state, WorkerState::Stopped);
    assert_eq!(p.polling, None);
    assert!(p.polls >= 1);
}

#[test]
fn a_profile_times_polls_by_spawn_location() {
    giong::block_on(async {
        let h = giong::handle();
        assert!(observe::take_profile().is_empty());
        h.set_profiling(true);
        assert!(h.is_profiling());
        let line = line!() + 1;
        let busy = giong::spawn(async {
            for _ in 0..20 {
                let start = Instant::now();
                while start.elapsed() < Duration::from_micros(200) {}
                giong::yield_now().await;
            }
        });
        busy.await.unwrap();
        h.set_profiling(false);
        // Workers notice at their next batch of tasks.
        giong::yield_now().await;
        let profile = observe::take_profile();
        let entry = profile
            .iter()
            .find(|p| here(p.location.file()) && p.location.line() == line)
            .unwrap_or_else(|| panic!("{profile:?}"));
        assert!(entry.polls >= 20, "{entry:?}");
        assert!(entry.total >= Duration::from_millis(4), "{entry:?}");
        assert!(entry.max >= Duration::from_micros(200), "{entry:?}");
        // Longest total first, and taken means gone.
        assert!(profile.windows(2).all(|w| w[0].total >= w[1].total));
        assert!(observe::take_profile().is_empty());

        // Off: nothing is recorded.
        giong::spawn(giong::yield_now()).await.unwrap();
        assert!(observe::take_profile().is_empty());
    });
}

struct Pool {
    open: std::cell::Cell<u64>,
}

impl Source for Pool {
    fn kind(&self) -> &'static str {
        "test pool"
    }
    fn name(&self) -> String {
        "db".into()
    }
    fn stats(&self) -> Vec<(&'static str, u64)> {
        vec![("open", self.open.get())]
    }
}

#[test]
fn sources_report_until_they_are_dropped() {
    giong::block_on(async {
        let pool = Rc::new(Pool {
            open: std::cell::Cell::new(3),
        });
        assert!(observe::register_rc(&pool));
        let snap = observe::snapshot();
        assert_eq!(snap.sources.len(), 1);
        assert_eq!(snap.sources[0].kind, "test pool");
        assert_eq!(snap.sources[0].name, "db");
        assert_eq!(snap.sources[0].stats, vec![("open", 3)]);
        pool.open.set(4);
        assert_eq!(observe::snapshot().sources[0].stats, vec![("open", 4)]);
        drop(pool);
        assert!(observe::snapshot().sources.is_empty());
    });
    // Outside a runtime there is nowhere to register.
    let pool = Rc::new(Pool {
        open: std::cell::Cell::new(0),
    });
    assert!(!observe::register_rc(&pool));
}

#[test]
#[should_panic(expected = "GIONG-E0008")]
fn pulse_of_a_worker_that_does_not_exist_panics() {
    let rt = giong::Builder::new().workers(1).build().unwrap();
    rt.handle().pulse(1);
}
