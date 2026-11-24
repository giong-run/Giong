//! The busy-poll mode (`Builder::spin_before_park`) must behave exactly like
//! the default mode, only faster to react.

use std::time::{Duration, Instant};

use giong::net::{TcpListener, TcpStream};
use giong::sync::mpsc;
use giong::time::{sleep, timeout};

mod common;

fn spinning(spin: Duration) -> giong::Builder {
    giong::Builder::new().spin_before_park(spin)
}

#[test]
fn io_works_while_spinning() {
    spinning(Duration::from_millis(5))
        .block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let server = giong::spawn(async move {
                let stream = listener.accept().await.unwrap();
                loop {
                    let data = stream.recv().await.unwrap();
                    if data.is_empty() {
                        break;
                    }
                    let reply = data.to_vec();
                    drop(data);
                    stream.write_all(reply).await.0.unwrap();
                }
            });
            let client = TcpStream::connect(addr).await.unwrap();
            for i in 0..200u32 {
                let msg = i.to_le_bytes().to_vec();
                client.write_all(msg.clone()).await.0.unwrap();
                let mut got = Vec::new();
                while got.len() < 4 {
                    got.extend_from_slice(&client.recv().await.unwrap());
                }
                assert_eq!(got, msg);
            }
            drop(client);
            server.await.unwrap();
        })
        .unwrap();
}

#[test]
fn timers_fire_on_time_while_spinning() {
    // The spin budget is longer than the timers: a due timer must end the spin.
    spinning(Duration::from_millis(500))
        .block_on(async {
            for ms in [1u64, 5, 20] {
                let start = Instant::now();
                sleep(Duration::from_millis(ms)).await;
                let elapsed = start.elapsed();
                assert!(
                    elapsed >= Duration::from_millis(ms),
                    "woke early: {elapsed:?}"
                );
                assert!(
                    elapsed < Duration::from_millis(ms + 100),
                    "woke late: {elapsed:?}"
                );
            }
            assert!(
                timeout(Duration::from_millis(10), sleep(Duration::from_secs(60)))
                    .await
                    .is_err()
            );
        })
        .unwrap();
}

#[test]
fn the_worker_still_sleeps_after_the_spin_budget() {
    // A short spin followed by a long wait: the timer must still fire.
    spinning(Duration::from_micros(100))
        .block_on(async {
            let start = Instant::now();
            sleep(Duration::from_millis(50)).await;
            let elapsed = start.elapsed();
            assert!(elapsed >= Duration::from_millis(50));
            assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
        })
        .unwrap();
}

#[test]
fn cross_thread_wakeups_reach_a_spinning_worker_and_a_sleeping_one() {
    let measured = common::wake_latency_within(Duration::from_millis(100), || {
        let (tx, mut rx) = mpsc::unbounded::<(u32, Instant)>();
        let sender = std::thread::spawn(move || {
            for i in 0..40u32 {
                // Gaps shorter and longer than the 2 ms spin budget, so the
                // message arrives both while the worker spins and after it
                // went to sleep.
                std::thread::sleep(Duration::from_micros(if i % 2 == 0 { 300 } else { 6_000 }));
                tx.send((i, Instant::now())).unwrap();
            }
        });
        let (count, worst) = spinning(Duration::from_millis(2))
            .block_on(async move {
                let mut count = 0;
                let mut worst = Duration::ZERO;
                while let Some((i, sent)) = timeout(Duration::from_secs(10), rx.recv())
                    .await
                    .expect("lost wake-up")
                {
                    assert_eq!(i, count);
                    count += 1;
                    worst = worst.max(sent.elapsed());
                }
                (count, worst)
            })
            .unwrap();
        sender.join().unwrap();
        assert_eq!(count, 40);
        worst
    });
    assert!(measured.is_ok(), "slow wake-up on every try: {measured:?}");
}

#[test]
fn spinning_workers_shut_down_cleanly() {
    let rt = spinning(Duration::from_millis(1))
        .workers(2)
        .build()
        .unwrap();
    let handle = rt.handle();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(30));
        handle.shutdown();
    });
    rt.run(|_| async {
        giong::shutdown_signal().await;
    })
    .unwrap();
    stopper.join().unwrap();
}

#[test]
fn a_zero_spin_duration_turns_the_mode_off() {
    spinning(Duration::ZERO)
        .block_on(async {
            sleep(Duration::from_millis(2)).await;
        })
        .unwrap();
}
