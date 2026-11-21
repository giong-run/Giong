//! Connection steering by receiving CPU (`GIONG_STEER=cpu`, a prototype).
//!
//! On loopback the kernel receives a SYN on the CPU of the thread that
//! connects, so a client pinned to the CPU of one worker must always reach
//! that worker's listener, and reach the others once that listener is gone.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use giong::net::TcpListener;
use giong::time::timeout;

mod common;

fn allowed_cpus() -> Vec<usize> {
    // SAFETY: a zeroed cpu_set_t of the size passed.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        assert_eq!(
            libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set),
            0
        );
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&c| libc::CPU_ISSET(c, &set))
            .collect()
    }
}

fn pin_thread(cpu: usize) {
    // SAFETY: a zeroed cpu_set_t with one CPU set.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        assert_eq!(
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set),
            0
        );
    }
}

fn ping(port: u16) {
    let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.write_all(b"x").unwrap();
    let mut b = [0u8];
    c.read_exact(&mut b).unwrap();
}

#[test]
#[cfg_attr(miri, ignore = "Miri has no SO_REUSEPORT")]
fn a_connection_goes_to_the_worker_on_the_cpu_that_received_it() {
    let cpus = allowed_cpus();
    if cpus.len() < 2 {
        eprintln!("skipped: needs two CPUs");
        return;
    }
    // Read once, before the first listener binds; this binary has one test.
    std::env::set_var("GIONG_STEER", "cpu");
    const WORKERS: usize = 2;
    let (_holder, port) = common::shared_port();
    let rt = giong::Builder::new()
        .workers(WORKERS)
        .pin_cores(true)
        .build()
        .unwrap();
    let handle = rt.handle();
    let ready = Arc::new(AtomicUsize::new(0));
    let served = Arc::new(Mutex::new(vec![0usize; WORKERS]));
    let drop_one = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));

    let (r, s, d, dd) = (
        ready.clone(),
        served.clone(),
        drop_one.clone(),
        dropped.clone(),
    );
    let server = std::thread::spawn(move || {
        rt.run(move |index| {
            let (ready, served, drop_one, dropped) = (r.clone(), s.clone(), d.clone(), dd.clone());
            async move {
                let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
                ready.fetch_add(1, Ordering::SeqCst);
                loop {
                    if index == 1 && drop_one.load(Ordering::SeqCst) {
                        drop(listener);
                        dropped.store(true, Ordering::SeqCst);
                        giong::shutdown_signal().await;
                        return;
                    }
                    let accepted =
                        giong::until_shutdown(timeout(Duration::from_millis(5), listener.accept()))
                            .await;
                    let stream = match accepted {
                        None => return,
                        Some(Err(_)) => continue,
                        Some(Ok(conn)) => conn.unwrap(),
                    };
                    served.lock().unwrap()[index] += 1;
                    giong::spawn(async move {
                        let data = stream.recv().await.unwrap();
                        let reply = data.to_vec();
                        drop(data);
                        stream.write_all(reply).await.0.unwrap();
                    });
                }
            }
        })
        .unwrap();
    });
    while ready.load(Ordering::SeqCst) < WORKERS {
        std::thread::sleep(Duration::from_millis(1));
    }

    // Worker 1 is pinned to the second allowed CPU.
    let cpu = cpus[1];
    let client = std::thread::spawn(move || {
        pin_thread(cpu);
        for _ in 0..40 {
            ping(port);
        }
    });
    client.join().unwrap();
    assert_eq!(*served.lock().unwrap(), vec![0, 40]);

    drop_one.store(true, Ordering::SeqCst);
    while !dropped.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let client = std::thread::spawn(move || {
        pin_thread(cpu);
        for _ in 0..20 {
            ping(port);
        }
    });
    client.join().unwrap();
    assert_eq!(*served.lock().unwrap(), vec![20, 40]);

    handle.shutdown();
    server.join().unwrap();
}
