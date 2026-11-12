//! `sync::oneshot` and `sync::mpsc::bounded`: on one worker, across workers
//! and from plain threads, including what happens when either end goes away
//! or a waiting send is cancelled.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use giong::sync::mpsc::{self, TrySendError};
use giong::sync::oneshot::{self, RecvError, TryRecvError};
use giong::time::{sleep, timeout};

/// Counts its live instances, to check that nothing leaks.
#[derive(Debug)]
struct Tracked(Arc<AtomicUsize>);

impl Tracked {
    fn new(live: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::SeqCst);
        Self(live.clone())
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

// ------------------------------------------------------------------ oneshot

#[test]
fn oneshot_delivers_one_value_on_one_worker() {
    giong::block_on(async {
        let (tx, rx) = oneshot::channel();
        giong::spawn(async move {
            sleep(Duration::from_millis(5)).await;
            tx.send("done").unwrap();
        });
        assert_eq!(rx.await, Ok("done"));
    });
}

#[test]
fn oneshot_delivers_from_another_thread() {
    giong::block_on(async {
        for i in 0..200 {
            let (tx, rx) = oneshot::channel();
            let thread = std::thread::spawn(move || tx.send(i).unwrap());
            assert_eq!(rx.await, Ok(i));
            thread.join().unwrap();
        }
    });
}

#[test]
fn oneshot_reports_a_sender_dropped_without_sending() {
    giong::block_on(async {
        let (tx, rx) = oneshot::channel::<u32>();
        giong::spawn(async move {
            sleep(Duration::from_millis(5)).await;
            drop(tx);
        });
        assert_eq!(rx.await, Err(RecvError));
    });
}

#[test]
fn oneshot_try_recv_sees_empty_value_and_closed() {
    let (tx, mut rx) = oneshot::channel();
    assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
    tx.send(7).unwrap();
    assert_eq!(rx.try_recv(), Ok(7));
    assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
}

#[test]
fn oneshot_sender_learns_that_nobody_is_listening() {
    giong::block_on(async {
        let (tx, rx) = oneshot::channel::<u32>();
        assert!(!tx.is_closed());
        giong::spawn(async move {
            sleep(Duration::from_millis(5)).await;
            drop(rx);
        });
        timeout(Duration::from_secs(10), tx.closed()).await.unwrap();
        assert!(tx.is_closed());
        assert_eq!(tx.send(1), Err(1));
    });
}

#[test]
fn oneshot_drops_a_value_nobody_received() {
    let live = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = oneshot::channel();
    tx.send(Tracked::new(&live)).unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 1);
    drop(rx);
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

#[test]
fn oneshot_receive_can_be_cancelled_and_resumed() {
    giong::block_on(async {
        let (tx, mut rx) = oneshot::channel();
        assert!(timeout(Duration::from_millis(5), &mut rx).await.is_err());
        tx.send(3).unwrap();
        assert_eq!(rx.await, Ok(3));
    });
}

// ------------------------------------------------------------------ bounded

#[test]
fn bounded_send_waits_while_the_channel_is_full() {
    giong::block_on(async {
        let (tx, mut rx) = mpsc::bounded(2);
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();
        assert!(matches!(tx.try_send(3), Err(TrySendError::Full(3))));
        assert!(timeout(Duration::from_millis(10), tx.send(3))
            .await
            .is_err());
        assert_eq!(rx.len(), 2);

        assert_eq!(rx.recv().await, Some(1));
        tx.send(3).await.unwrap();
        assert_eq!(rx.recv().await, Some(2));
        assert_eq!(rx.recv().await, Some(3));
        assert!(rx.is_empty());
    });
}

#[test]
fn bounded_waiting_senders_get_room_in_arrival_order() {
    giong::block_on(async {
        let (tx, mut rx) = mpsc::bounded(1);
        tx.send(0).await.unwrap();
        let mut tasks = Vec::new();
        for i in 1..=5 {
            let tx = tx.clone();
            tasks.push(giong::spawn(async move { tx.send(i).await.unwrap() }));
            // Let each one start waiting before the next is spawned.
            giong::yield_now().await;
        }
        drop(tx);
        let mut got = Vec::new();
        while let Some(i) = rx.recv().await {
            got.push(i);
        }
        assert_eq!(got, [0, 1, 2, 3, 4, 5]);
        for task in tasks {
            task.await.unwrap();
        }
    });
}

#[test]
fn bounded_a_new_sender_does_not_jump_the_line() {
    giong::block_on(async {
        let (tx, mut rx) = mpsc::bounded(1);
        tx.send(0).await.unwrap();
        let waiting = tx.clone();
        let task = giong::spawn(async move { waiting.send(1).await.unwrap() });
        giong::yield_now().await;
        // A slot frees up and is promised to the waiting sender ...
        assert_eq!(rx.try_recv(), Some(0));
        // ... so a newcomer cannot take it.
        assert!(matches!(tx.try_send(2), Err(TrySendError::Full(2))));
        task.await.unwrap();
        assert_eq!(rx.recv().await, Some(1));
    });
}

#[test]
fn bounded_a_cancelled_send_gives_its_place_to_the_next() {
    giong::block_on(async {
        let (tx, mut rx) = mpsc::bounded(1);
        tx.send(0).await.unwrap();

        // Gives up while still in line.
        assert!(timeout(Duration::from_millis(5), tx.send(99))
            .await
            .is_err());

        // Two more wait; the first is granted a slot but aborted before it
        // can use it, so the slot must go to the second.
        let first = {
            let tx = tx.clone();
            giong::spawn(async move { tx.send(1).await.unwrap() })
        };
        giong::yield_now().await;
        let second = {
            let tx = tx.clone();
            giong::spawn(async move { tx.send(2).await.unwrap() })
        };
        giong::yield_now().await;
        assert_eq!(rx.try_recv(), Some(0));
        first.abort();
        second.await.unwrap();
        assert_eq!(rx.recv().await, Some(2));
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(rx.is_empty());
    });
}

#[test]
fn bounded_senders_get_their_value_back_when_the_receiver_goes() {
    giong::block_on(async {
        let (tx, rx) = mpsc::bounded(1);
        tx.send(String::from("queued")).await.unwrap();
        let waiting = {
            let tx = tx.clone();
            giong::spawn(async move { tx.send(String::from("waiting")).await })
        };
        giong::yield_now().await;
        drop(rx);
        assert_eq!(waiting.await.unwrap().unwrap_err().0, "waiting");
        assert!(tx.is_closed());
        assert!(matches!(
            tx.try_send(String::new()),
            Err(TrySendError::Closed(_))
        ));
    });
}

#[test]
fn bounded_receiver_drains_then_sees_the_end() {
    let live = Arc::new(AtomicUsize::new(0));
    giong::block_on(async {
        let (tx, mut rx) = mpsc::bounded(4);
        for _ in 0..3 {
            tx.send(Tracked::new(&live)).await.unwrap();
        }
        drop(tx);
        let mut n = 0;
        while let Some(item) = rx.recv().await {
            drop(item);
            n += 1;
        }
        assert_eq!(n, 3);
        // Values left in a dropped channel are dropped too.
        let (tx, rx) = mpsc::bounded(4);
        tx.try_send(Tracked::new(&live)).unwrap();
        drop(rx);
    });
    assert_eq!(live.load(Ordering::SeqCst), 0);
}

#[test]
fn bounded_carries_values_between_workers() {
    const WORKERS: usize = 4;
    const PER_WORKER: usize = 2_000;
    let (tx, rx) = mpsc::bounded::<(usize, usize)>(8);
    let rx = Arc::new(Mutex::new(Some(rx)));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    giong::Builder::new()
        .workers(WORKERS)
        .build()
        .unwrap()
        .run(move |worker| {
            let (tx, rx, seen) = (tx.clone(), rx.clone(), seen2.clone());
            async move {
                let producer = giong::spawn(async move {
                    for i in 0..PER_WORKER {
                        tx.send((worker, i)).await.unwrap();
                    }
                });
                if worker == 0 {
                    let mut rx = rx.lock().unwrap().take().unwrap();
                    let mut got = Vec::with_capacity(WORKERS * PER_WORKER);
                    while got.len() < WORKERS * PER_WORKER {
                        got.push(rx.recv().await.unwrap());
                    }
                    *seen.lock().unwrap() = got;
                }
                producer.await.unwrap();
            }
        })
        .unwrap();
    let seen = seen.lock().unwrap();
    // Every value arrived once, and each worker's values in order.
    for worker in 0..WORKERS {
        let mine: Vec<usize> = seen
            .iter()
            .filter(|(w, _)| *w == worker)
            .map(|(_, i)| *i)
            .collect();
        assert_eq!(mine, (0..PER_WORKER).collect::<Vec<_>>());
    }
}

#[test]
fn bounded_works_with_values_that_are_not_send_on_one_worker() {
    giong::block_on(async {
        let (tx, mut rx) = mpsc::bounded(1);
        let shared = Rc::new(RefCell::new(0));
        let s = shared.clone();
        giong::spawn(async move {
            for _ in 0..10 {
                tx.send(s.clone()).await.unwrap();
            }
        });
        while let Some(rc) = rx.recv().await {
            *rc.borrow_mut() += 1;
        }
        assert_eq!(*shared.borrow(), 10);
    });
}

#[test]
#[should_panic(expected = "capacity of at least 1")]
fn bounded_refuses_a_capacity_of_zero() {
    let _ = mpsc::bounded::<()>(0);
}
