//! A multi-worker runtime and UDP under the simulation, and replay of a
//! seed in a fresh process.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use giong_sim::{assert_deterministic, Faults, Sim};

/// Three workers share a port; worker 0 runs 30 clients against it.
pub fn three_workers() {
    let served = Arc::new(AtomicUsize::new(0));
    let s2 = served.clone();
    let rt = giong::Builder::new().workers(3).build().unwrap();
    rt.run(move |index| {
        let served = s2.clone();
        async move {
            let listener = giong::net::TcpListener::bind("127.0.0.1:7000").unwrap();
            giong::spawn(async move {
                loop {
                    let Ok(stream) = listener.accept().await else {
                        continue;
                    };
                    let served = served.clone();
                    giong::spawn(async move {
                        // Messages are 9 bytes; a read may return fewer.
                        let mut got = Vec::new();
                        while got.len() < 9 {
                            let (res, buf) = stream.read(vec![0u8; 64]).await;
                            match res {
                                Ok(n) if n > 0 => got.extend_from_slice(&buf[..n]),
                                _ => return,
                            }
                        }
                        if stream.write_all(got).await.0.is_ok() {
                            served.fetch_add(1, Ordering::SeqCst);
                        }
                    });
                }
            });
            if index != 0 {
                giong::shutdown_signal().await;
                return;
            }
            let mut tasks = Vec::new();
            for i in 0..30u32 {
                tasks.push(giong::spawn(async move {
                    let c = giong::net::TcpStream::connect("127.0.0.1:7000")
                        .await
                        .unwrap();
                    let msg = format!("hello {i:03}").into_bytes();
                    let (res, _) = c.write_all(msg.clone()).await;
                    res.unwrap();
                    let mut got = Vec::new();
                    while got.len() < msg.len() {
                        let (res, buf) = c.read(vec![0u8; 64]).await;
                        let n = res.unwrap();
                        assert!(n > 0, "closed early");
                        got.extend_from_slice(&buf[..n]);
                    }
                    assert_eq!(got, msg);
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
            giong::handle().shutdown();
        }
    })
    .unwrap();
    assert_eq!(served.load(Ordering::SeqCst), 30);
}

#[test]
fn three_workers_are_deterministic() {
    let faults = Faults {
        preempt: 0.1,
        ..Faults::benign()
    };
    let r = assert_deterministic(21, faults, three_workers);
    eprintln!("{} steps, {:?} virtual", r.steps, r.virtual_time);
}

#[test]
fn three_workers_sweep() {
    let faults = Faults {
        preempt: 0.2,
        ..Faults::benign()
    };
    let reports = giong_sim::sweep(0..20, faults, |_| three_workers());
    let distinct: std::collections::HashSet<u64> = reports.iter().map(|r| r.hash).collect();
    assert!(distinct.len() > 15, "seeds should give different runs");
}

fn udp_ping() -> usize {
    giong::block_on(async {
        let a = giong::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let b = giong::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let (aa, ba) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        let echo = giong::spawn(async move {
            loop {
                let (d, from) = b.recv_from().await.unwrap();
                if d.as_slice() == b"stop" {
                    break;
                }
                let (r, _) = b.send_to(d.as_slice().to_vec(), from).await;
                r.unwrap();
            }
        });
        let mut answered = 0;
        for i in 0..50u32 {
            let (r, _) = a.send_to(i.to_be_bytes().to_vec(), ba).await;
            r.unwrap();
            if let Ok(Ok((d, from))) =
                giong::time::timeout(Duration::from_millis(100), a.recv_from()).await
            {
                assert_eq!(from, ba);
                assert_eq!(d.len(), 4);
                answered += 1;
            }
        }
        let _ = aa;
        // "stop" may be lost: send it until the echo task ends.
        loop {
            let (r, _) = a.send_to(b"stop".to_vec(), ba).await;
            r.unwrap();
            giong::time::sleep(Duration::from_millis(10)).await;
            if echo.is_finished() {
                break;
            }
        }
        answered
    })
}

#[test]
fn udp_loses_reorders_and_duplicates() {
    let ((), r) = Sim::new(5)
        .faults(Faults {
            udp_loss: 0.2,
            udp_dup: 0.1,
            udp_reorder: 0.2,
            ..Faults::none()
        })
        .run(|| {
            let n = udp_ping();
            assert!(n < 50 && n > 10, "answered {n}");
        });
    assert!(
        r.injected.get("udp-loss").copied().unwrap_or(0) > 0,
        "{r:?}"
    );
}

/// The hash of seed 99 must be the same in a fresh process (as when
/// replaying with GIONG_SIM_SEED) and after other runs in this one.
#[test]
fn a_seed_replays_in_a_fresh_process() {
    let hostile = |_| {
        let _ = std::panic::catch_unwind(three_workers);
    };
    if std::env::var("GIONG_SIM_CHILD").is_ok() {
        let r = &giong_sim::sweep(0..1, Faults::hostile(), hostile)[0];
        println!("HASH={}", r.hash);
        return;
    }
    // Other seeds first, so this process is no longer fresh.
    let warm = giong_sim::sweep(97..100, Faults::hostile(), hostile)
        .pop()
        .unwrap();
    assert_eq!(warm.seed, 99);
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_seed_replays_in_a_fresh_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("GIONG_SIM_CHILD", "1")
        .env("GIONG_SIM_SEED", "99")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let cold: u64 = text
        .lines()
        .find_map(|l| l.split("HASH=").nth(1))
        .unwrap_or_else(|| {
            panic!(
                "no hash from the child: {text}\n{}",
                String::from_utf8_lossy(&out.stderr)
            )
        })
        .parse()
        .unwrap();
    assert_eq!(cold, warm.hash);
}
