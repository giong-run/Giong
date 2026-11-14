//! Executor behaviour: spawning, joining, panics, aborts, fairness.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use giong::time::sleep;

#[test]
fn block_on_returns_value() {
    assert_eq!(giong::block_on(async { 6 * 7 }), 42);
}

#[test]
fn block_on_can_borrow_from_the_stack() {
    let mut counter = 0;
    giong::block_on(async {
        giong::yield_now().await;
        counter += 1;
    });
    assert_eq!(counter, 1);
}

#[test]
fn spawn_and_join() {
    let out = giong::block_on(async {
        let a = giong::spawn(async { 1 });
        let b = giong::spawn(async {
            giong::yield_now().await;
            2
        });
        a.await.unwrap() + b.await.unwrap()
    });
    assert_eq!(out, 3);
}

#[test]
fn tasks_are_not_send_and_can_share_rc() {
    let total = giong::block_on(async {
        let shared = Rc::new(RefCell::new(Vec::new()));
        let mut handles = Vec::new();
        for i in 0..100 {
            let shared = shared.clone();
            handles.push(giong::spawn(async move {
                giong::yield_now().await;
                shared.borrow_mut().push(i);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let sum: i32 = shared.borrow().iter().sum();
        sum
    });
    assert_eq!(total, 4950);
}

#[test]
#[cfg_attr(miri, ignore = "minutes long under Miri")]
fn many_tasks() {
    let n = giong::block_on(async {
        let counter = Rc::new(Cell::new(0u32));
        let handles: Vec<_> = (0..20_000)
            .map(|_| {
                let counter = counter.clone();
                giong::spawn(async move {
                    giong::yield_now().await;
                    counter.set(counter.get() + 1);
                })
            })
            .collect();
        for h in handles {
            h.await.unwrap();
        }
        counter.get()
    });
    assert_eq!(n, 20_000);
}

#[test]
fn panic_in_task_is_reported_and_does_not_kill_the_worker() {
    let (first, second) = giong::block_on(async {
        let bad = giong::spawn(async {
            giong::yield_now().await;
            panic!("boom");
        });
        let first = bad.await;
        // The worker is still alive and able to run more tasks.
        let second = giong::spawn(async { 7 }).await;
        (first, second)
    });
    let err = first.unwrap_err();
    assert!(err.is_panic());
    let payload = err.try_into_panic().unwrap();
    assert_eq!(*payload.downcast_ref::<&str>().unwrap(), "boom");
    assert_eq!(second.unwrap(), 7);
}

#[test]
#[should_panic(expected = "main exploded")]
fn panic_in_main_future_propagates() {
    giong::block_on(async {
        giong::yield_now().await;
        panic!("main exploded");
    });
}

#[test]
fn abort_cancels_a_task_and_runs_its_destructors() {
    struct SetOnDrop(Rc<Cell<bool>>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    let dropped = Rc::new(Cell::new(false));
    let flag = dropped.clone();
    let result = giong::block_on(async move {
        let guard = SetOnDrop(flag);
        let handle = giong::spawn(async move {
            let _guard = guard;
            sleep(Duration::from_secs(3600)).await;
            1
        });
        giong::yield_now().await;
        handle.abort();
        handle.await
    });
    assert!(result.unwrap_err().is_cancelled());
    assert!(dropped.get());
}

#[test]
fn abort_after_completion_is_a_noop() {
    giong::block_on(async {
        let handle = giong::spawn(async { 5 });
        giong::yield_now().await;
        giong::yield_now().await;
        assert!(handle.is_finished());
        handle.abort();
        assert_eq!(handle.await.unwrap(), 5);
    });
}

#[test]
fn detached_tasks_are_dropped_when_the_runtime_ends() {
    struct SetOnDrop(Rc<Cell<bool>>);
    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    let dropped = Rc::new(Cell::new(false));
    let flag = dropped.clone();
    giong::block_on(async move {
        let guard = SetOnDrop(flag);
        giong::spawn(async move {
            let _guard = guard;
            sleep(Duration::from_secs(3600)).await;
        });
        giong::yield_now().await;
    });
    assert!(dropped.get());
}

#[test]
fn yield_now_lets_other_tasks_run() {
    let order = giong::block_on(async {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        let other = giong::spawn(async move {
            l.borrow_mut().push("other");
        });
        log.borrow_mut().push("main-before");
        giong::yield_now().await;
        log.borrow_mut().push("main-after");
        other.await.unwrap();
        let order = log.borrow().clone();
        order
    });
    assert_eq!(order, vec!["main-before", "other", "main-after"]);
}

#[test]
fn runtimes_can_run_one_after_another_on_the_same_thread() {
    for i in 0..5 {
        let out = giong::block_on(async move {
            sleep(Duration::from_millis(1)).await;
            giong::spawn(async move { i * 2 }).await.unwrap()
        });
        assert_eq!(out, i * 2);
    }
}

#[test]
#[should_panic(expected = "already running")]
fn nested_block_on_is_rejected() {
    giong::block_on(async {
        giong::block_on(async {});
    });
}

#[test]
fn no_leaks_after_tasks_finish() {
    giong::block_on(async {
        for _ in 0..100 {
            giong::spawn(async { giong::yield_now().await })
                .await
                .unwrap();
        }
        let stats = giong::stats();
        assert_eq!(stats.tasks, 0, "{stats:?}");
        assert_eq!(stats.timers, 0, "{stats:?}");
        assert_eq!(stats.tracked_ops, 0, "{stats:?}");
    });
}
