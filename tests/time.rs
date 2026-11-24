//! Timers.

use std::cell::RefCell;
use std::future::Future;
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

/// The worker reads the clock once per turn of its loop and shares it with
/// the timers polled in that turn. A zero timeout made late in a long turn
/// must still end at its first poll, not a turn later.
#[test]
fn a_zero_timeout_late_in_a_long_turn_ends_at_once() {
    giong::block_on(async {
        // A timer polled early in the turn: the worker reads its clock.
        let mut early = std::pin::pin!(sleep(Duration::from_secs(60)));
        std::future::poll_fn(|cx| {
            let _ = early.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
        // The turn goes on for longer than a timer tick.
        let busy = Instant::now();
        while busy.elapsed() < Duration::from_millis(5) {
            std::hint::spin_loop();
        }
        // Not ready at its first poll: only the timeout can end it then.
        assert_eq!(
            timeout(Duration::ZERO, giong::yield_now()).await,
            Err(Elapsed)
        );
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
        assert_eq!(giong::observe::worker_stats().timers, 0);
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
        assert_eq!(giong::observe::worker_stats().timers, 0);
    });
}

#[test]
#[cfg_attr(miri, ignore = "minutes long under Miri")]
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
        assert_eq!(giong::observe::worker_stats().timers, 0);
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

/// A sleep started just after a timer fired ends at its deadline, not a
/// whole millisecond later: the wheel used to round every deadline up to
/// the next millisecond tick, and the event loop wakes just after a tick,
/// so `loop { sleep(2 ms).await }` took 3 ms a turn (#228).
///
/// Checked on Linux only: macOS applies its own timer slack to `kevent`'s
/// timeout (on CI's virtual machines a 2 ms sleep took 2.0 to 3.8 ms, median
/// 3.0 ms), which this test cannot tell from the tick it looks for. The test
/// is kept by name on every system, as the suite's index expects.
#[cfg(unix)]
#[test]
#[cfg_attr(
    miri,
    ignore = "timing: measures how late sleeps end, which Miri slows down"
)]
fn a_chain_of_sleeps_is_not_a_tick_late_each_time() {
    #[cfg(not(target_os = "linux"))]
    eprintln!("skipped: the system adds its own timer slack");
    #[cfg(target_os = "linux")]
    giong::block_on(async {
        let mut took = Vec::new();
        for _ in 0..21 {
            let start = Instant::now();
            sleep(Duration::from_millis(2)).await;
            took.push(start.elapsed());
        }
        took.sort();
        let median = took[10];
        assert!(
            took[0] >= Duration::from_millis(2),
            "a sleep ended early: {took:?}"
        );
        assert!(
            median < Duration::from_micros(2_700),
            "a 2 ms sleep took {median:?} (median of 21; all: {took:?})"
        );
    });
}

/// With precise timers on, short and long sleeps still last at least their
/// duration and not far longer, on every driver (only kqueue acts on it).
#[test]
fn precise_timers_keep_sleeps_at_least_their_duration() {
    giong::Builder::new()
        .workers(1)
        .precise_timers(true)
        .build()
        .unwrap()
        .run(|_| async {
            for ms in [2, 2, 5, 150] {
                let d = Duration::from_millis(ms);
                let start = Instant::now();
                sleep(d).await;
                let elapsed = start.elapsed();
                assert!(elapsed >= d, "{d:?} woke early: {elapsed:?}");
                assert!(
                    elapsed < d + Duration::from_millis(500),
                    "{d:?} woke far too late: {elapsed:?}"
                );
            }
        })
        .unwrap();
}

/// Short sleeps end within a few milliseconds of their deadline. On Windows
/// the completion port's own timeout ends on the system's 15.6 ms clock
/// tick, where a 1 ms sleep took 15.6 ms; a high-resolution timer ends the
/// wait there now. Measured on CI: about 2 ms on Linux and macOS. A
/// wall-clock bound means nothing under Miri, which slows every turn of the
/// loop: merge queue runs measured a 13 ms median there.
#[test]
#[cfg_attr(
    miri,
    ignore = "timing: measures how late sleeps end, which Miri slows down"
)]
fn a_short_sleep_is_not_rounded_up_to_a_clock_tick() {
    #[cfg(windows)]
    if under_wine() {
        // Wine has no high-resolution timer packets: the tick stays.
        return;
    }
    giong::block_on(async {
        let mut took = Vec::new();
        for _ in 0..21 {
            let start = Instant::now();
            sleep(Duration::from_millis(1)).await;
            took.push(start.elapsed());
        }
        took.sort();
        let median = took[10];
        assert!(
            median >= Duration::from_millis(1) && median < Duration::from_millis(8),
            "a 1 ms sleep took {median:?} (median of 21; all: {took:?})"
        );
    });
}

#[cfg(windows)]
fn under_wine() -> bool {
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    // SAFETY: looks up an export of a module every process has.
    unsafe {
        let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
        !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
    }
}
