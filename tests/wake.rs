//! Cross-thread wake-ups of a task that waits on lock-free state.
//!
//! A thread outside the runtime publishes an event with a plain atomic store
//! and then wakes the task; the poll that follows must see the event. Until
//! 0.3 the executor cleared a task's "queued" flag with a plain store, and
//! about 70 in a million of these wake-ups were lost on x86: the task slept
//! forever. `src/notify.rs` has the loom model; this is the same race on real
//! hardware. It takes about 10 seconds, so it is not part of the default run:
//!
//! ```text
//! cargo test --release --test wake -- --ignored
//! ```

use std::future::poll_fn;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

const ITERATIONS: u64 = 300_000;

fn race(spin: bool) -> u64 {
    let event = Arc::new(AtomicU64::new(0));
    let seen = Arc::new(AtomicU64::new(0));
    let slot: Arc<Mutex<Option<Waker>>> = Arc::new(Mutex::new(None));

    let waker_thread = {
        let (event, seen, slot) = (event.clone(), seen.clone(), slot.clone());
        std::thread::spawn(move || {
            let waker = loop {
                if let Some(waker) = slot.lock().unwrap().clone() {
                    break waker;
                }
                std::thread::yield_now();
            };
            let mut lost = 0;
            let mut rng: u64 = 0x9e37_79b9_7f4a_7c15;
            for i in 1..=ITERATIONS {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                // A first wake-up gets the task polled; the event lands at a
                // random moment during that poll.
                waker.wake_by_ref();
                for _ in 0..rng % 2000 {
                    std::hint::spin_loop();
                }
                event.store(i, Ordering::Release);
                waker.wake_by_ref();
                let start = Instant::now();
                while seen.load(Ordering::Acquire) < i {
                    if start.elapsed() > Duration::from_millis(200) {
                        lost += 1;
                        // Recover, so one lost wake-up does not hang the test.
                        waker.wake_by_ref();
                        while seen.load(Ordering::Acquire) < i {
                            std::hint::spin_loop();
                        }
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
            lost
        })
    };

    let mut builder = giong::Builder::new().workers(1);
    if spin {
        builder = builder.spin_before_park(Duration::from_micros(50));
    }
    builder
        .block_on(async move {
            let mut registered = false;
            poll_fn(|cx| {
                if !registered {
                    registered = true;
                    *slot.lock().unwrap() = Some(cx.waker().clone());
                }
                // No lock in the poll: a locked instruction here would hide
                // the race on x86.
                let now = event.load(Ordering::Acquire);
                if now > seen.load(Ordering::Relaxed) {
                    seen.store(now, Ordering::Release);
                }
                if now == ITERATIONS {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await
        })
        .unwrap();
    waker_thread.join().unwrap()
}

#[test]
#[ignore = "takes about 10 seconds; run with --ignored"]
fn no_wake_up_is_lost_while_parked() {
    assert_eq!(race(false), 0, "wake-ups lost");
}

#[test]
#[ignore = "takes about 10 seconds; run with --ignored"]
fn no_wake_up_is_lost_while_spinning() {
    assert_eq!(race(true), 0, "wake-ups lost");
}
