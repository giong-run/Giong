//! `giong::observe`: pulses read from other threads, snapshots, profiles,
//! sources, and tasks named by where they were spawned.

use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::observe::{self, Source, WorkerState};

fn here(file: &str) -> bool {
    file.ends_with("tests/observe.rs")
}

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
            let line = line!() + 1;
            let blocker = handle.spawn_on(1, || async {
                // Blocks the thread: the mistake the pulse must expose.
                std::thread::sleep(Duration::from_millis(600));
            });
            // Wait until worker 1 is in it, then check that nothing moves.
            let start = Instant::now();
            let first = loop {
                let p = handle.pulse(1);
                if p.polling
                    .is_some_and(|l| here(l.file()) && l.line() == line)
                {
                    break p;
                }
                assert!(start.elapsed() < Duration::from_secs(5), "{p:?}");
                giong::time::sleep(Duration::from_millis(5)).await;
            };
            giong::time::sleep(Duration::from_millis(200)).await;
            let later = handle.pulse(1);
            assert_eq!(later.state, WorkerState::Running);
            assert_eq!(later.polls, first.polls);
            assert_eq!(later.ticks, first.ticks);
            let location = later.polling.expect("polling");
            assert_eq!(location.line(), line);
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
