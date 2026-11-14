//! The thread-per-core runtime: workers, cross-thread wake-ups, shutdown.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use giong::net::TcpListener;
use giong::sync::mpsc;
use giong::time::{sleep, timeout};

mod common;

#[test]
fn every_worker_runs_its_main_future_on_its_own_thread() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    giong::Builder::new()
        .workers(4)
        .build()
        .unwrap()
        .run(move |index| {
            let s = s.clone();
            async move {
                assert_eq!(giong::worker_id(), index);
                sleep(Duration::from_millis(5)).await;
                s.lock().unwrap().push((index, std::thread::current().id()));
            }
        })
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.iter().map(|(i, _)| *i).collect::<HashSet<_>>(),
        (0..4).collect()
    );
    assert_eq!(
        seen.iter().map(|(_, t)| *t).collect::<HashSet<_>>().len(),
        4
    );
}

#[test]
fn shutdown_from_outside_wakes_every_worker() {
    let rt = giong::Builder::new().workers(3).build().unwrap();
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
    assert!(start.elapsed() >= Duration::from_millis(50));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn until_shutdown_interrupts_a_pending_future() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    rt.run(|index| async move {
        if index == 0 {
            sleep(Duration::from_millis(20)).await;
            giong::handle().shutdown();
        }
        let res = giong::until_shutdown(sleep(Duration::from_secs(3600))).await;
        assert!(res.is_none());
        assert!(giong::handle().is_shutdown());
        // Once shut down, it resolves immediately.
        assert!(giong::until_shutdown(sleep(Duration::from_secs(3600)))
            .await
            .is_none());
    })
    .unwrap();
}

#[test]
fn spawn_on_runs_work_on_the_chosen_worker_and_returns_its_result() {
    const WORKERS: usize = 4;
    let rt = giong::Builder::new().workers(WORKERS).build().unwrap();
    let handle = rt.handle();
    let results = Arc::new(Mutex::new(Vec::new()));
    let r = results.clone();
    rt.run(move |index| {
        let handle = handle.clone();
        let results = r.clone();
        async move {
            if index == 0 {
                // Ask every worker (including ourselves) to run a job.
                let jobs: Vec<_> = (0..WORKERS)
                    .map(|target| {
                        handle.spawn_on(target, move || async move {
                            sleep(Duration::from_millis(1)).await;
                            (target, giong::worker_id())
                        })
                    })
                    .collect();
                for job in jobs {
                    let result = job.await.unwrap();
                    results.lock().unwrap().push(result);
                }
                handle.shutdown();
            }
            giong::shutdown_signal().await;
        }
    })
    .unwrap();
    let results = results.lock().unwrap();
    assert_eq!(*results, (0..WORKERS).map(|w| (w, w)).collect::<Vec<_>>());
}

#[test]
fn spawn_on_a_worker_that_does_not_exist_panics_with_its_code() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    let payload =
        std::panic::catch_unwind(AssertUnwindSafe(|| handle.spawn_on(2, || async {}))).unwrap_err();
    let message = payload.downcast_ref::<String>().unwrap();
    assert!(
        message.starts_with("GIONG-E0008: spawn_on was given worker 2, but the runtime has 2"),
        "{message}"
    );
}

#[test]
fn a_remote_task_is_awaited_from_a_plain_thread() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    let outside = std::thread::spawn(move || {
        let task = handle.spawn_on(1, || async {
            sleep(Duration::from_millis(5)).await;
            giong::worker_id() + 10
        });
        fn send_and_sync<T: Send + Sync>(_: &T) {}
        send_and_sync(&task);
        let value = wait(task);
        handle.shutdown();
        value
    });
    rt.run(|_| giong::shutdown_signal()).unwrap();
    assert_eq!(outside.join().unwrap().unwrap(), 11);
}

#[test]
fn aborting_a_remote_task_drops_it_on_its_own_worker_first() {
    struct OnDrop(Arc<Mutex<Option<usize>>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            *self.0.lock().unwrap() = Some(giong::worker_id());
        }
    }

    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    let checked = Arc::new(AtomicUsize::new(0));
    let c = checked.clone();
    rt.run(move |index| {
        let (handle, checked) = (handle.clone(), c.clone());
        async move {
            if index == 0 {
                // Aborted while it waits.
                let dropped_on = Arc::new(Mutex::new(None));
                let d = dropped_on.clone();
                let (started_tx, started_rx) = giong::sync::oneshot::channel();
                let task = handle.spawn_on(1, move || async move {
                    let _guard = OnDrop(d);
                    let _ = started_tx.send(());
                    sleep(Duration::from_secs(3600)).await;
                    "finished"
                });
                started_rx.await.unwrap();
                assert!(!task.is_finished());
                task.abort();
                let result = timeout(Duration::from_secs(5), task).await.unwrap();
                assert!(result.unwrap_err().is_cancelled());
                // The future was dropped on worker 1 before the handle resolved.
                assert_eq!(*dropped_on.lock().unwrap(), Some(1));
                checked.fetch_add(1, Ordering::SeqCst);

                // Aborted at once: before or after worker 1 starts it, it is
                // never allowed to finish.
                let ran = Arc::new(AtomicUsize::new(0));
                let r = ran.clone();
                let task = handle.spawn_on(1, move || async move {
                    sleep(Duration::from_millis(50)).await;
                    r.fetch_add(1, Ordering::SeqCst);
                });
                task.abort();
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                sleep(Duration::from_millis(100)).await;
                assert_eq!(ran.load(Ordering::SeqCst), 0);
                checked.fetch_add(1, Ordering::SeqCst);
                handle.shutdown();
            }
            giong::shutdown_signal().await;
        }
    })
    .unwrap();
    assert_eq!(checked.load(Ordering::SeqCst), 2);
}

#[test]
fn a_remote_task_is_cancelled_when_its_worker_stops() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    let left = Arc::new(Mutex::new(None));
    let l = left.clone();
    let h = handle.clone();
    rt.run(move |index| {
        let (handle, left) = (h.clone(), l.clone());
        async move {
            if index == 0 {
                let task = handle.spawn_on(1, || async {
                    sleep(Duration::from_secs(3600)).await;
                });
                // Wait until worker 1 runs it, then stop everything.
                handle.spawn_on(1, || async {}).await.unwrap();
                *left.lock().unwrap() = Some(task);
                handle.shutdown();
            }
            giong::shutdown_signal().await;
        }
    })
    .unwrap();
    let task = left.lock().unwrap().take().unwrap();
    assert!(task.is_finished());
    assert!(wait(task).unwrap_err().is_cancelled());

    // The runtime has stopped: a new job is refused, and its handle says so.
    let late = handle.spawn_on(0, || async { 1 });
    assert!(late.is_finished());
    assert!(wait(late).unwrap_err().is_cancelled());
}

#[test]
fn work_sent_to_a_stopped_runtime_is_cancelled() {
    // spawn_balanced after run returned: the job used to wait in the queue
    // for as long as a Handle existed.
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    rt.run(|_| async {}).unwrap();
    let job = handle.spawn_balanced(|| async { 1 });
    assert!(job.is_finished());
    assert!(wait(job).unwrap_err().is_cancelled());
    let job = handle.spawn_on(1, || async { 1 });
    assert!(wait(job).unwrap_err().is_cancelled());

    // A runtime dropped without running: work already sent to it, and work
    // sent afterwards.
    let rt = giong::Builder::new().workers(1).build().unwrap();
    let handle = rt.handle();
    let early_on = handle.spawn_on(0, || async { 1 });
    let early_balanced = handle.spawn_balanced(|| async { 1 });
    drop(rt);
    assert!(early_on.is_finished() && early_balanced.is_finished());
    assert!(wait(early_on).unwrap_err().is_cancelled());
    assert!(wait(early_balanced).unwrap_err().is_cancelled());
    assert!(wait(handle.spawn_balanced(|| async { 1 }))
        .unwrap_err()
        .is_cancelled());

    // The handle of a block_on runtime, used after block_on returned.
    let handle = giong::block_on(async { giong::handle() });
    let job = handle.spawn_balanced(|| async { 1 });
    assert!(job.is_finished());
    assert!(wait(job).unwrap_err().is_cancelled());
}

/// Blocks the current (plain) thread on a future.
fn wait<F: std::future::Future>(fut: F) -> F::Output {
    struct Unpark(std::thread::Thread);
    impl std::task::Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = std::task::Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = std::task::Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let std::task::Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

#[test]
fn channel_delivers_across_workers_and_from_plain_threads() {
    const PER_SENDER: usize = 10_000;
    let (tx, rx) = mpsc::channel::<usize>();
    let rx = Arc::new(Mutex::new(Some(rx)));
    // One sender for each of workers 1 and 2, handed out by index.
    let senders = Arc::new(Mutex::new(vec![None, Some(tx.clone()), Some(tx.clone())]));
    let total = Arc::new(AtomicUsize::new(0));
    let t = total.clone();

    // A sender that is not a runtime thread at all.
    let outside = std::thread::spawn(move || {
        for i in 0..PER_SENDER {
            tx.send(i).unwrap();
            if i % 1000 == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    });

    giong::Builder::new()
        .workers(3)
        .build()
        .unwrap()
        .run(move |index| {
            let (rx, senders, total) = (rx.clone(), senders.clone(), t.clone());
            async move {
                if index == 0 {
                    // Receiver: sleeps in io_uring between messages and must be
                    // woken by other threads through the eventfd.
                    let mut rx = rx.lock().unwrap().take().unwrap();
                    let mut sum = 0;
                    while let Some(v) = rx.recv().await {
                        sum += v;
                    }
                    total.store(sum, Ordering::SeqCst);
                } else {
                    let tx = senders.lock().unwrap()[index].take().unwrap();
                    for i in 0..PER_SENDER {
                        tx.send(i).unwrap();
                        if i % 500 == 0 {
                            sleep(Duration::from_micros(200)).await;
                        }
                    }
                }
            }
        })
        .unwrap();
    outside.join().unwrap();
    let per_sender: usize = (0..PER_SENDER).sum();
    assert_eq!(total.load(Ordering::SeqCst), 3 * per_sender);
}

#[test]
fn reuseport_listeners_on_every_worker_serve_connections() {
    const WORKERS: usize = 3;
    const CLIENTS: usize = 60;
    let (_holder, port) = common::shared_port();
    let rt = giong::Builder::new().workers(WORKERS).build().unwrap();
    let handle = rt.handle();
    let ready = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(vec![0usize; WORKERS]));

    let (r, s) = (ready.clone(), served.clone());
    let server = std::thread::spawn(move || {
        rt.run(move |index| {
            let (ready, served) = (r.clone(), s.clone());
            async move {
                let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
                ready.fetch_add(1, Ordering::SeqCst);
                while let Some(conn) = giong::until_shutdown(listener.accept()).await {
                    let stream = conn.unwrap();
                    let served = served.clone();
                    giong::spawn(async move {
                        let data = stream.recv().await.unwrap();
                        let reply = data.to_vec();
                        drop(data);
                        stream.write_all(reply).await.0.unwrap();
                        served.lock().unwrap()[index] += 1;
                    });
                }
            }
        })
        .unwrap();
    });

    while ready.load(Ordering::SeqCst) < WORKERS {
        std::thread::sleep(Duration::from_millis(1));
    }
    for i in 0..CLIENTS {
        let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let msg = format!("client-{i}");
        c.write_all(msg.as_bytes()).unwrap();
        let mut buf = vec![0u8; msg.len()];
        c.read_exact(&mut buf).unwrap();
        assert_eq!(buf, msg.as_bytes());
    }
    handle.shutdown();
    server.join().unwrap();
    let served = served.lock().unwrap();
    assert_eq!(served.iter().sum::<usize>(), CLIENTS);
}

#[test]
fn a_panicking_worker_stops_the_runtime_and_propagates() {
    let result = std::panic::catch_unwind(|| {
        giong::Builder::new()
            .workers(3)
            .build()
            .unwrap()
            .run(|index| async move {
                if index == 1 {
                    sleep(Duration::from_millis(10)).await;
                    panic!("worker one is down");
                }
                giong::shutdown_signal().await;
            })
            .unwrap();
    });
    let payload = result.unwrap_err();
    assert_eq!(
        *payload.downcast_ref::<&str>().unwrap(),
        "worker one is down"
    );
}

#[test]
fn sigterm_triggers_a_graceful_shutdown() {
    // Runs in a child process so the signal cannot disturb the other tests.
    if std::env::var_os("GIONG_SIGNAL_CHILD").is_some() {
        giong::Builder::new()
            .workers(2)
            .shutdown_on_signals(true)
            .build()
            .unwrap()
            .run(|index| async move {
                if index == 0 {
                    println!("ready");
                }
                giong::shutdown_signal().await;
            })
            .unwrap();
        println!("clean exit");
        return;
    }
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "sigterm_triggers_a_graceful_shutdown",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GIONG_SIGNAL_CHILD", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        lines.by_ref().any(|l| l.unwrap().contains("ready")),
        "child never became ready"
    );
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let rest: Vec<String> = lines.map(|l| l.unwrap()).collect();
    assert!(child.wait().unwrap().success());
    assert!(
        rest.iter().any(|l| l.contains("clean exit")),
        "child output: {rest:?}"
    );
}

#[test]
fn sigterm_still_works_after_worker_0_returned() {
    // Signals used to be watched by a task on worker 0 only: once worker 0's
    // main future had returned, SIGTERM and SIGINT did nothing while the
    // other workers kept running.
    if std::env::var_os("GIONG_SIGNAL_CHILD_0").is_some() {
        giong::Builder::new()
            .workers(2)
            .shutdown_on_signals(true)
            .build()
            .unwrap()
            .run(|index| async move {
                if index == 0 {
                    return;
                }
                // Let worker 0 finish first.
                sleep(Duration::from_millis(50)).await;
                println!("ready");
                giong::shutdown_signal().await;
            })
            .unwrap();
        println!("clean exit");
        return;
    }
    use std::io::BufRead;
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().unwrap();
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "sigterm_still_works_after_worker_0_returned",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GIONG_SIGNAL_CHILD_0", "1")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(
        lines.by_ref().any(|l| l.unwrap().contains("ready")),
        "child never became ready"
    );
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    // The child must exit by itself; kill it if it does not.
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > Duration::from_secs(20) {
            child.kill().unwrap();
            child.wait().unwrap();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let rest: Vec<String> = lines.map(|l| l.unwrap()).collect();
    assert!(status.is_some(), "SIGTERM was ignored: {rest:?}");
    assert!(status.unwrap().success());
    assert!(
        rest.iter().any(|l| l.contains("clean exit")),
        "child output: {rest:?}"
    );
}

#[test]
fn a_sleeping_worker_is_woken_promptly_by_another_thread() {
    // Latency of the eventfd wake-up path.
    let (tx, mut rx) = mpsc::channel::<Instant>();
    let sender = std::thread::spawn(move || {
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(1));
            tx.send(Instant::now()).unwrap();
        }
    });
    let worst = giong::block_on(async move {
        let mut worst = Duration::ZERO;
        while let Some(sent) = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("lost wake-up")
        {
            worst = worst.max(sent.elapsed());
        }
        worst
    });
    sender.join().unwrap();
    assert!(
        worst < Duration::from_millis(100),
        "slow cross-thread wake-up: {worst:?}"
    );
}

#[test]
fn a_panicking_spawn_on_job_does_not_kill_the_worker() {
    let rt = giong::Builder::new().workers(2).build().unwrap();
    let handle = rt.handle();
    let done = Arc::new(AtomicUsize::new(0));
    let d = done.clone();
    rt.run(move |index| {
        let (handle, done) = (handle.clone(), d.clone());
        async move {
            if index == 0 {
                // The closure that builds the future blows up on worker 1 ...
                let failed = handle
                    .spawn_on(1, || -> std::future::Ready<()> { panic!("factory failed") })
                    .await;
                let payload = failed.unwrap_err().try_into_panic().unwrap();
                assert_eq!(payload.downcast_ref::<&str>(), Some(&"factory failed"));
                // ... and so does a task ...
                let fail = Arc::new(AtomicUsize::new(1));
                let failed = handle
                    .spawn_on(1, move || async move {
                        sleep(Duration::from_millis(1)).await;
                        assert_eq!(fail.load(Ordering::SeqCst), 0, "task failed");
                    })
                    .await;
                assert!(failed.unwrap_err().is_panic());
                // ... and worker 1 is still alive to run the next one.
                let id = handle.spawn_on(1, || async { giong::worker_id() }).await;
                done.store(id.unwrap() + 10, Ordering::SeqCst);
                handle.shutdown();
            }
            giong::shutdown_signal().await;
        }
    })
    .unwrap();
    assert_eq!(done.load(Ordering::SeqCst), 11);
}
