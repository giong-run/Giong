//! The simulator itself: time, threads, determinism, a TCP round trip.

use std::time::{Duration, Instant};

use giong_sim::{assert_deterministic, Faults, Sim};

#[test]
fn virtual_time_passes_without_real_time() {
    let real = Instant::now();
    let ((), report) = Sim::new(1).run(|| {
        giong::block_on(async {
            let start = Instant::now();
            giong::time::sleep(Duration::from_secs(3600)).await;
            assert!(start.elapsed() >= Duration::from_secs(3600));
        });
    });
    assert!(
        report.virtual_time >= Duration::from_secs(3600),
        "{report:?}"
    );
    assert!(real.elapsed() < Duration::from_secs(10));
}

#[test]
fn threads_take_turns() {
    let ((), r) = Sim::new(7)
        .faults(Faults {
            preempt: 0.3,
            ..Faults::none()
        })
        .run(|| {
            let hs: Vec<_> = (0..4)
                .map(|i| {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_millis(10 * i));
                        i
                    })
                })
                .collect();
            let sum: u64 = hs.into_iter().map(|h| h.join().unwrap()).sum();
            assert_eq!(sum, 6);
        });
    assert!(r.steps > 0);
}

fn echo() {
    giong::block_on(async {
        let listener = giong::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = giong::spawn(async move {
            let stream = listener.accept().await.unwrap();
            let mut total = 0;
            loop {
                let buf = vec![0u8; 4096];
                let (res, buf) = stream.read(buf).await;
                let n = res.unwrap();
                if n == 0 {
                    break;
                }
                total += n;
                let (res, _) = stream.write_all(buf[..n].to_vec()).await;
                res.unwrap();
            }
            total
        });
        let client = giong::net::TcpStream::connect(addr).await.unwrap();
        let data: Vec<u8> = (0..100_000u32).map(|i| i as u8).collect();
        let (res, _) = client.write_all(data.clone()).await;
        res.unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut got = Vec::new();
        loop {
            let (res, buf) = client.read(vec![0u8; 8192]).await;
            let n = res.unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, data);
        assert_eq!(server.await.unwrap(), data.len());
    });
}

#[test]
fn tcp_echo_without_faults() {
    let ((), r) = Sim::new(3).run(echo);
    eprintln!("{} steps, {:?} virtual", r.steps, r.virtual_time);
}

#[test]
fn tcp_echo_is_deterministic_under_benign_faults() {
    let r = assert_deterministic(11, Faults::benign(), echo);
    eprintln!("{} steps, injected {:?}", r.steps, r.injected);
    assert!(!r.injected.is_empty());
}
