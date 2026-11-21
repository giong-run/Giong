//! Budgets: limits per tenant on tasks, memory, I/O in flight and compute
//! closures, and the compute lane's fair queues.

use std::cell::RefCell;
use std::future::Future;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use giong::budget::{self, Budget, Limits};
use giong::error::{self, Code};
use giong::net::{TcpListener, TcpStream};
use giong::time::sleep;
use giong::DriverKind;

/// Polls `fut` once; `Some` if it finished.
fn poll_once<F: Future + Unpin>(fut: &mut F) -> Option<F::Output> {
    struct Noop;
    impl std::task::Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = std::task::Waker::from(Arc::new(Noop));
    match std::pin::Pin::new(fut).poll(&mut Context::from_waker(&waker)) {
        Poll::Ready(v) => Some(v),
        Poll::Pending => None,
    }
}

async fn settle() {
    for _ in 0..20 {
        giong::yield_now().await;
    }
    sleep(Duration::from_millis(5)).await;
}

#[test]
fn a_budget_and_its_reservations_can_be_sent_to_other_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Budget>();
    assert_send_sync::<budget::Reservation>();
}

#[test]
fn try_spawn_refuses_at_the_task_limit_with_e0080() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().tasks(2));
        let (tx, rx) = giong::sync::oneshot::channel::<()>();
        let first = b
            .try_spawn(async move {
                let _ = rx.await;
            })
            .unwrap();
        let second = b.try_spawn(sleep(Duration::from_millis(1))).unwrap();
        let err = b.try_spawn(async {}).unwrap_err();
        assert_eq!(error::code(&err), Some(Code::BudgetFull));
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);
        assert!(err.to_string().contains("budget \"t\""), "{err}");
        assert_eq!(b.stats().tasks.used, 2);
        assert_eq!(b.stats().tasks.refused, 1);
        second.await.unwrap();
        // The finished task gave its slot back.
        assert_eq!(b.stats().tasks.used, 1);
        b.try_spawn(async {}).unwrap().await.unwrap();
        drop(tx);
        first.await.unwrap();
        assert_eq!(b.stats().tasks.used, 0);
        assert_eq!(
            b.stats().memory.used,
            0,
            "task futures are charged and given back"
        );
    });
}

#[test]
fn spawn_waits_for_a_slot_and_slots_go_in_order() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().tasks(1));
        let order = Rc::new(RefCell::new(Vec::new()));
        let blocker = b.spawn(sleep(Duration::from_millis(30))).await;
        let mut waiters = Vec::new();
        for i in 0..3 {
            let (b, order) = (b.clone(), order.clone());
            waiters.push(giong::spawn(async move {
                let t = b.spawn(async {}).await;
                order.borrow_mut().push(i);
                t.await.unwrap();
            }));
            giong::yield_now().await;
        }
        settle().await;
        assert!(
            order.borrow().is_empty(),
            "nobody gets a slot while it is taken"
        );
        assert_eq!(b.stats().tasks.waiting, 3);
        blocker.await.unwrap();
        for w in waiters {
            w.await.unwrap();
        }
        assert_eq!(*order.borrow(), vec![0, 1, 2]);
        assert_eq!(b.stats().tasks.used, 0);
        assert_eq!(b.stats().tasks.waited, 3);
    });
}

#[test]
fn tasks_spawned_inside_a_budget_are_charged_to_it() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().tasks(1));
        let (tx, rx) = giong::sync::oneshot::channel::<()>();
        // The parent returns its child's handle, which is a future.
        #[allow(clippy::async_yields_async)]
        let parent = b
            .spawn(async move {
                assert_eq!(budget::current().unwrap().name(), "t");
                // Over the limit: a plain spawn cannot wait, so it is
                // charged anyway.
                let child = giong::spawn(async move {
                    assert_eq!(budget::current().unwrap().name(), "t");
                    let _ = rx.await;
                });
                child
            })
            .await;
        let child = parent.await.unwrap();
        assert_eq!(b.stats().tasks.used, 1, "the child holds the budget's task");
        assert!(b.try_spawn(async {}).is_err());
        drop(tx);
        child.await.unwrap();
        assert_eq!(b.stats().tasks.used, 0);
        assert!(budget::current().is_none(), "outside the task, no budget");
    });
}

#[test]
fn reserve_waits_and_a_large_reservation_is_not_overtaken() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().memory(1000));
        let held = b.try_reserve(900).unwrap();
        assert_eq!(b.stats().memory.used, 900);
        let err = b.try_reserve(200).unwrap_err();
        assert_eq!(error::code(&err), Some(Code::BudgetFull));

        let mut big = pin!(b.reserve(500));
        assert!(poll_once(&mut big).is_none());
        // 50 bytes would fit, but the larger reservation asked first.
        let err = b.try_reserve(50).unwrap_err();
        assert_eq!(error::code(&err), Some(Code::BudgetFull));
        drop(held);
        let big = big.await.unwrap();
        assert_eq!(big.bytes(), 500);
        assert_eq!(b.stats().memory.used, 500);
        drop(big);
        assert_eq!(b.stats().memory.used, 0);
        assert_eq!(b.stats().memory.peak, 900);
    });
}

#[test]
fn a_reservation_larger_than_the_budget_fails_at_once_with_e0081() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().memory(100));
        let err = b.reserve(101).await.unwrap_err();
        assert_eq!(error::code(&err), Some(Code::BudgetTooSmall));
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = b.try_reserve(101).unwrap_err();
        assert_eq!(error::code(&err), Some(Code::BudgetTooSmall));
    });
}

#[test]
fn a_dropped_waiting_reservation_leaves_the_queue() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().memory(100));
        let mut held = b.try_reserve(100).unwrap();
        let mut first = Box::pin(b.reserve(80));
        assert!(poll_once(&mut first).is_none());
        let mut second = Box::pin(b.reserve(20));
        assert!(poll_once(&mut second).is_none());
        assert_eq!(b.stats().memory.waiting, 2);
        drop(first);
        assert_eq!(b.stats().memory.waiting, 1);
        held.release(30);
        // The second one now leads, and fits.
        let second = second.await.unwrap();
        assert_eq!(b.stats().memory.used, 90);
        drop((held, second));
        assert_eq!(b.stats().memory.used, 0);
    });
}

#[test]
fn a_granted_but_dropped_reservation_gives_its_bytes_back() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().memory(100));
        let held = b.try_reserve(100).unwrap();
        let mut waiting = Box::pin(b.reserve(60));
        assert!(poll_once(&mut waiting).is_none());
        drop(held); // hands the 60 bytes to the waiter
        assert_eq!(b.stats().memory.used, 60);
        drop(waiting); // never polled again
        assert_eq!(b.stats().memory.used, 0);
    });
}

#[test]
fn raising_a_limit_lets_waiting_calls_through() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().memory(10));
        let _held = b.try_reserve(10).unwrap();
        let mut waiting = Box::pin(b.reserve(10));
        assert!(poll_once(&mut waiting).is_none());
        b.set_limits(Limits::new().memory(20));
        assert_eq!(b.limits().memory(20), b.limits());
        let r = waiting.await.unwrap();
        assert_eq!(r.bytes(), 10);
    });
}

/// A connected pair; nothing is ever written to the second.
async fn pair() -> (TcpStream, std::net::TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = std::net::TcpStream::connect(addr).unwrap();
    let stream = listener.accept().await.unwrap();
    (stream, peer)
}

#[test]
fn io_in_flight_is_limited_and_a_cancelled_read_holds_its_slot_until_the_kernel_is_done() {
    giong::block_on(async {
        let (stream, mut peer) = pair().await;
        let stream = Rc::new(stream);
        let b = Budget::new("t", Limits::new().io(1));
        let s = stream.clone();
        let reader = b
            .spawn(async move { s.read(vec![0u8; 16]).await.0.unwrap() })
            .await;
        settle().await;
        let s = stream.clone();
        let writer = b
            .spawn(async move { s.write_all(b"hello".to_vec()).await.0.unwrap() })
            .await;
        settle().await;
        let io = b.stats().io;
        if giong::driver_kind() == DriverKind::IoUring {
            // The read waits in the kernel; the write waits for its slot.
            assert_eq!((io.used, io.waiting), (1, 1), "{io:?}");
            assert!(!writer.is_finished());
            // Cancel the read: the kernel still holds it until its
            // cancellation completes, and so does the budget.
            reader.abort();
            assert_eq!(b.stats().io.used, 1, "released before the kernel was done");
        } else {
            reader.abort();
        }
        writer.await.unwrap();
        settle().await;
        assert_eq!(b.stats().io.used, 0);
        let mut got = [0u8; 5];
        std::io::Read::read_exact(&mut peer, &mut got).unwrap();
        assert_eq!(&got, b"hello");
        assert!(b.stats().io.peak <= 1);
    });
}

#[test]
fn enter_charges_a_futures_io_without_a_task() {
    giong::block_on(async {
        let (stream, mut peer) = pair().await;
        let b = Budget::new("t", Limits::new());
        b.enter(async {
            assert_eq!(budget::current().unwrap().name(), "t");
            stream.write_all(b"x".to_vec()).await.0.unwrap();
        })
        .await;
        assert!(budget::current().is_none());
        assert_eq!(b.stats().tasks.peak, 0);
        let mut got = [0u8; 1];
        std::io::Read::read_exact(&mut peer, &mut got).unwrap();
        assert_eq!(b.stats().io.used, 0);
    });
}

#[test]
fn compute_closures_are_limited_and_charged_until_they_return() {
    giong::block_on(async {
        let b = Budget::new("t", Limits::new().compute(1));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let job = b.try_spawn_compute(move || rx.recv().ok()).unwrap();
        let err = b.try_spawn_compute(|| ()).unwrap_err();
        assert_eq!(error::code(&err), Some(Code::BudgetFull));
        let mut waiting = Box::pin(b.spawn_compute(|| 7));
        assert!(poll_once(&mut waiting).is_none());
        tx.send(()).unwrap();
        job.await.unwrap();
        assert_eq!(waiting.await.await.unwrap(), 7);
        sleep(Duration::from_millis(5)).await;
        assert_eq!(b.stats().compute.used, 0);

        // Inside a budgeted task, the plain functions use the budget.
        let inner = b
            .spawn(async {
                let (tx, rx) = std::sync::mpsc::channel::<()>();
                let job = giong::spawn_compute(move || rx.recv().ok());
                let refused = giong::try_spawn_compute(|| ()).unwrap_err();
                assert_eq!(error::code(&refused), Some(Code::BudgetFull));
                assert_eq!(budget::current().unwrap().stats().compute.used, 1);
                tx.send(()).unwrap();
                job.await.unwrap();
            })
            .await;
        inner.await.unwrap();
    });
}

#[test]
fn the_compute_lane_does_not_make_a_light_tenant_wait_behind_a_heavy_one() {
    let rt = giong::Builder::new()
        .workers(1)
        .compute_threads(1)
        .build()
        .unwrap();
    rt.run(|_| async {
        let heavy = Budget::new("heavy", Limits::new());
        let light = Budget::new("light", Limits::new());
        let done = Arc::new(Mutex::new(Vec::new()));
        let spin = |ms: u64| {
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(ms) {
                std::hint::spin_loop();
            }
        };
        let mut jobs = Vec::new();
        for i in 0..10 {
            let done = done.clone();
            jobs.push(
                heavy
                    .spawn_compute(move || {
                        spin(20);
                        done.lock().unwrap().push(format!("heavy{i}"));
                    })
                    .await,
            );
        }
        // Let the heavy tenant's first job run so its queue is behind.
        sleep(Duration::from_millis(30)).await;
        let started = Instant::now();
        let d = done.clone();
        light
            .spawn_compute(move || {
                spin(1);
                d.lock().unwrap().push("light".into());
            })
            .await
            .await
            .unwrap();
        let waited = started.elapsed();
        for j in jobs {
            j.await.unwrap();
        }
        let done = done.lock().unwrap();
        let at = done.iter().position(|s| s == "light").unwrap();
        // FIFO would put it last (index 10); fair queues after at most the
        // heavy job running and the one it is charged for.
        assert!(at <= 3, "light job finished at {at}: {done:?}");
        assert!(
            waited < Duration::from_millis(100),
            "light job waited {waited:?}"
        );
    })
    .unwrap();
}

#[test]
fn compute_weight_shares_the_lane() {
    let rt = giong::Builder::new()
        .workers(1)
        .compute_threads(1)
        .build()
        .unwrap();
    rt.run(|_| async {
        let a = Budget::new("a", Limits::new().compute_weight(3));
        let b = Budget::new("b", Limits::new());
        let done = Arc::new(Mutex::new(Vec::new()));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        // Hold the only thread while both queues fill.
        let gate = giong::spawn_compute(move || gate_rx.recv().ok());
        sleep(Duration::from_millis(10)).await;
        let mut jobs = Vec::new();
        for i in 0..12 {
            for (name, budget) in [("a", &a), ("b", &b)] {
                let done = done.clone();
                jobs.push(
                    budget
                        .spawn_compute(move || {
                            // Sleeps rather than spins: the lane charges wall
                            // time, which a busy machine would stretch for
                            // spinning jobs unevenly.
                            std::thread::sleep(Duration::from_millis(2));
                            done.lock().unwrap().push((name, i));
                        })
                        .await,
                );
            }
        }
        gate_tx.send(()).unwrap();
        gate.await.unwrap();
        for j in jobs {
            j.await.unwrap();
        }
        let done = done.lock().unwrap();
        let first: Vec<_> = done.iter().take(12).map(|(n, _)| *n).collect();
        let a_first = first.iter().filter(|n| **n == "a").count();
        // Weight 3 to 1: about 9 of the first 12.
        assert!((7..=11).contains(&a_first), "{first:?}");
    })
    .unwrap();
}

#[test]
fn work_outside_any_budget_is_not_charged() {
    giong::block_on(async {
        assert!(budget::current().is_none());
        let b = Budget::new("t", Limits::new());
        giong::spawn(async {}).await.unwrap();
        giong::spawn_compute(|| ()).await.unwrap();
        assert_eq!(b.stats().tasks.peak, 0);
        assert_eq!(b.stats().compute.peak, 0);
    });
}
