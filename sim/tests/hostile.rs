//! Hostile faults against the runtime's own sockets: every run must end,
//! with errors reported as errors and nothing left behind.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use giong_sim::{Faults, Sim};

/// Two workers serve an echo; worker 0 runs 20 clients that tolerate
/// errors. Returns (ok, failed) client counts.
fn tolerant_echo() -> (usize, usize) {
    let ok = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicUsize::new(0));
    let (ok2, failed2) = (ok.clone(), failed.clone());
    giong::Builder::new()
        .workers(2)
        .build()
        .unwrap()
        .run(move |index| {
            let (ok, failed) = (ok2.clone(), failed2.clone());
            async move {
                let listener = giong::net::TcpListener::bind("127.0.0.1:7100").unwrap();
                giong::spawn(async move {
                    loop {
                        let Ok(stream) = listener.accept().await else {
                            continue;
                        };
                        giong::spawn(async move {
                            loop {
                                let (res, buf) = stream.read(vec![0u8; 1024]).await;
                                match res {
                                    Ok(n) if n > 0 => {
                                        if stream.write_all(buf[..n].to_vec()).await.0.is_err() {
                                            return;
                                        }
                                    }
                                    _ => return,
                                }
                            }
                        });
                    }
                });
                if index != 0 {
                    giong::shutdown_signal().await;
                    return;
                }
                let tasks: Vec<_> = (0..20u32)
                    .map(|i| {
                        giong::spawn(async move {
                            let c = giong::net::TcpStream::connect("127.0.0.1:7100").await?;
                            let msg = vec![i as u8; 3000];
                            c.write_all(msg.clone()).await.0?;
                            let mut got = 0;
                            while got < msg.len() {
                                let (res, buf) = c.read(vec![0u8; 1024]).await;
                                let n = res?;
                                if n == 0 {
                                    return Err(std::io::Error::other("closed early"));
                                }
                                assert!(buf[..n].iter().all(|b| *b == i as u8), "corrupt data");
                                got += n;
                            }
                            Ok::<_, std::io::Error>(())
                        })
                    })
                    .collect();
                for t in tasks {
                    let r = giong::time::timeout(Duration::from_secs(600), t).await;
                    match r
                        .expect("client hung for 10 minutes")
                        .expect("client panicked")
                    {
                        Ok(()) => ok.fetch_add(1, Ordering::SeqCst),
                        Err(_) => failed.fetch_add(1, Ordering::SeqCst),
                    };
                }
                giong::handle().shutdown();
            }
        })
        .unwrap();
    (ok.load(Ordering::SeqCst), failed.load(Ordering::SeqCst))
}

#[test]
fn hostile_sweep_always_ends() {
    let seeds = std::env::var("GIONG_SIM_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100u64);
    let mut total = (0, 0);
    let mut injected = std::collections::BTreeMap::new();
    for r in giong_sim::sweep(0..seeds, Faults::hostile(), |_| {
        let (ok, failed) = tolerant_echo();
        assert_eq!(ok + failed, 20);
    }) {
        for (k, v) in r.injected {
            *injected.entry(k).or_insert(0) += v;
        }
        total.0 += 1;
        total.1 += r.steps;
    }
    eprintln!("{} runs, {} steps, injected {injected:?}", total.0, total.1);
}

#[test]
fn benign_faults_never_show_as_errors() {
    for r in giong_sim::sweep(1000..1050, Faults::benign(), |_| {
        let (ok, failed) = tolerant_echo();
        assert_eq!((ok, failed), (20, 0));
    }) {
        assert!(r.steps > 0);
    }
}

#[test]
fn a_partition_makes_connects_time_out() {
    let (timed_out, r) = Sim::new(4)
        .partition("127.0.0.1".parse().unwrap(), "127.0.0.2".parse().unwrap())
        .run(|| {
            giong::block_on(async {
                let _l = giong::net::TcpListener::bind("127.0.0.2:80").unwrap();
                let start = std::time::Instant::now();
                let r = giong::time::timeout(
                    Duration::from_secs(10),
                    giong::net::TcpStream::connect("127.0.0.2:80"),
                )
                .await;
                (r.is_err(), start.elapsed())
            })
        });
    assert!(timed_out.0, "connect across a partition must not succeed");
    assert!(timed_out.1 >= Duration::from_secs(10));
    assert!(r.injected.is_empty());
}
