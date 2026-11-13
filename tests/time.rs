//! Timers.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use giong::time::{sleep, sleep_until, timeout, Elapsed};

#[test]
fn sleep_waits_at_least_the_duration() {
    giong::block_on(async {
        let start = Instant::now();
        sleep(Duration::from_millis(50)).await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(50),
            "woke early: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_millis(500),
            "woke far too late: {elapsed:?}"
        );
    });
}

#[test]
fn zero_and_past_deadlines_complete_immediately() {
    giong::block_on(async {
        let start = Instant::now();
        sleep(Duration::ZERO).await;
        sleep_until(Instant::now() - Duration::from_secs(1)).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    });
}

#[test]
fn timers_fire_in_deadline_order() {
    let order = giong::block_on(async {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut handles = Vec::new();
        // Deadlines relative to one base instant comfortably in the future, so
        // every timer is armed before the first one is due — however slowly the
        // tasks start (e.g. under valgrind).
        let base = Instant::now() + Duration::from_millis(250);
        for ms in [40u64, 10, 30, 20, 50] {
            let log = log.clone();
            handles.push(giong::spawn(async move {
                sleep_until(base + Duration::from_millis(ms)).await;
                log.borrow_mut().push(ms);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let order = log.borrow().clone();
        order
    });
    assert_eq!(order, vec![10, 20, 30, 40, 50]);
}

#[test]
fn timeout_elapses() {
    giong::block_on(async {
        let start = Instant::now();
        let res = timeout(Duration::from_millis(30), sleep(Duration::from_secs(60))).await;
        assert_eq!(res, Err(Elapsed));
        assert!(start.elapsed() < Duration::from_millis(500));
        // The inner sleep was dropped and must have unregistered itself.
        assert_eq!(giong::stats().timers, 0);
    });
}

#[test]
fn timeout_passes_the_value_through() {
    giong::block_on(async {
        let res = timeout(Duration::from_secs(60), async {
            sleep(Duration::from_millis(5)).await;
            "done"
        })
        .await;
        assert_eq!(res, Ok("done"));
        assert_eq!(giong::stats().timers, 0);
    });
}

#[test]
fn many_timers() {
    giong::block_on(async {
        let start = Instant::now();
        let handles: Vec<_> = (0..5_000u64)
            .map(|i| {
                giong::spawn(async move { sleep(Duration::from_micros(1_000 + i * 10)).await })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        assert!(start.elapsed() >= Duration::from_micros(1_000 + 4_999 * 10));
        assert_eq!(giong::stats().timers, 0);
    });
}

#[test]
fn sleep_can_be_reset() {
    giong::block_on(async {
        let mut s = sleep(Duration::from_secs(3600));
        s.reset(Instant::now() + Duration::from_millis(10));
        let start = Instant::now();
        s.await;
        assert!(start.elapsed() < Duration::from_millis(500));
    });
}

/// `timeout` adds a timer to the future it wraps, and no second copy of it.
#[test]
fn a_timeout_is_as_large_as_its_future_plus_a_timer() {
    type Big = std::future::Ready<[u8; 4096]>;
    let size = std::mem::size_of::<giong::time::Timeout<Big>>();
    let parts = std::mem::size_of::<Big>() + std::mem::size_of::<giong::time::Sleep>();
    assert!(size <= parts + 8, "{size} bytes for {parts} bytes of parts");
}
