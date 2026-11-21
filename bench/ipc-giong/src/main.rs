//! giong::ipc against giong's UnixStream between two processes, each one
//! giong worker pinned to its own CPU.
//!
//! * `ping`: one 64-byte message in flight; round trips per second and CPU
//!   per round trip (both processes).
//! * `stream`: 64-byte messages one way as fast as they are taken. IPC sends
//!   each message on its own; the Unix socket gets 32 messages per write
//!   (2 KiB), as a program would batch them.
//!
//!   bench-ipc-giong <ipc|unix> <ping|stream> <count> <cpu-a> <cpu-b>
//!
//! GIONG_SPIN_US sets `spin_before_park` on both workers.
use std::process::Command;
use std::time::{Duration, Instant};

use giong::ipc;
use giong::net::{UnixListener, UnixStream};

const MSG: usize = 64;
const BATCH: usize = 32;

fn builder() -> giong::Builder {
    let mut b = giong::Builder::new().workers(1);
    if let Some(us) = std::env::var("GIONG_SPIN_US")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        b = b.spin_before_park(Duration::from_micros(us));
    }
    b
}

fn pin(cpu: usize) {
    // SAFETY: a zeroed cpu_set_t with one CPU set.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

fn cpu_us() -> f64 {
    usage(libc::RUSAGE_SELF)
}

/// The child's whole CPU time, start-up included (small next to the run).
fn children_cpu_us() -> f64 {
    usage(libc::RUSAGE_CHILDREN)
}

fn usage(who: i32) -> f64 {
    let mut total = 0.0;
    {
        // SAFETY: a zeroed rusage for the kernel to fill.
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(who, &mut ru) };
        total += (ru.ru_utime.tv_sec + ru.ru_stime.tv_sec) as f64 * 1e6
            + (ru.ru_utime.tv_usec + ru.ru_stime.tv_usec) as f64;
    }
    total
}

/// The child: connects and answers (ping) or drains (stream).
async fn child(transport: &str, mode: &str, count: u64, path: &str) {
    match transport {
        "ipc" => {
            let (mut tx, mut rx) = ipc::connect(path, 1 << 20).await.unwrap();
            let mut got = 0u64;
            while let Some(m) = rx.recv().await.unwrap() {
                if mode == "ping" {
                    let mut buf = [0u8; MSG];
                    buf.copy_from_slice(&m);
                    drop(m);
                    tx.send(&buf).await.unwrap();
                } else {
                    drop(m);
                    got += 1;
                    if got == count {
                        tx.send(b"done").await.unwrap();
                    }
                }
            }
        }
        _ => {
            let s = UnixStream::connect(path).await.unwrap();
            let mut got = 0usize;
            let total = count as usize * MSG;
            loop {
                let chunk = s.recv().await.unwrap();
                if chunk.is_empty() {
                    return;
                }
                got += chunk.len();
                drop(chunk);
                if mode == "ping" {
                    while got >= MSG {
                        got -= MSG;
                        s.write_all(vec![7u8; MSG]).await.0.unwrap();
                    }
                } else if got == total {
                    s.write_all(b"done".to_vec()).await.0.unwrap();
                }
            }
        }
    }
}

async fn parent(
    transport: &str,
    mode: &str,
    count: u64,
    path: &str,
    child_cpu: usize,
) -> (f64, f64) {
    let exe = std::env::current_exe().unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let spawn = || {
        Command::new(&exe)
            .args(&args)
            .env("BENCH_CHILD", path)
            .env("BENCH_CHILD_CPU", child_cpu.to_string())
            .spawn()
            .unwrap()
    };
    let msg = [7u8; MSG];
    match transport {
        "ipc" => {
            let l = ipc::IpcListener::bind(path, 1 << 20).unwrap();
            let mut proc = spawn();
            let (mut tx, mut rx) = l.accept().await.unwrap();
            let (c0, t0) = (cpu_us(), Instant::now());
            if mode == "ping" {
                for _ in 0..count {
                    tx.send(&msg).await.unwrap();
                    drop(rx.recv().await.unwrap().unwrap());
                }
            } else {
                for _ in 0..count {
                    tx.send(&msg).await.unwrap();
                }
                drop(rx.recv().await.unwrap().unwrap());
            }
            let r = (t0.elapsed().as_secs_f64(), cpu_us() - c0);
            drop(tx);
            drop(rx);
            proc.wait().unwrap();
            (r.0, r.1 + children_cpu_us())
        }
        _ => {
            let l = UnixListener::bind(path).unwrap();
            let mut proc = spawn();
            let s = l.accept().await.unwrap();
            let (c0, t0) = (cpu_us(), Instant::now());
            if mode == "ping" {
                for _ in 0..count {
                    s.write_all(msg.to_vec()).await.0.unwrap();
                    let mut got = 0;
                    while got < MSG {
                        got += s.recv().await.unwrap().len();
                    }
                }
            } else {
                let batch = vec![7u8; MSG * BATCH];
                for _ in 0..count / BATCH as u64 {
                    s.write_all(batch.clone()).await.0.unwrap();
                }
                let _ = s.recv().await.unwrap();
            }
            let r = (t0.elapsed().as_secs_f64(), cpu_us() - c0);
            // A dropped socket is closed through the ring later; the child
            // must see the end now, before this thread blocks in wait.
            s.shutdown(std::net::Shutdown::Both).unwrap();
            drop(s);
            proc.wait().unwrap();
            (r.0, r.1 + children_cpu_us())
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (transport, mode) = (args[1].clone(), args[2].clone());
    let count: u64 = args[3].parse().unwrap();
    if let Ok(path) = std::env::var("BENCH_CHILD") {
        pin(std::env::var("BENCH_CHILD_CPU").unwrap().parse().unwrap());
        builder()
            .block_on(async move { child(&transport, &mode, count, &path).await })
            .unwrap();
        return;
    }
    pin(args[4].parse().unwrap());
    let child_cpu: usize = args[5].parse().unwrap();
    let path = format!("/tmp/giong-bench-ipc-{}.sock", std::process::id());
    let _ = std::fs::remove_file(&path);
    let p = path.clone();
    let (t, c) = builder()
        .block_on(async move { parent(&transport, &mode, count, &p, child_cpu).await })
        .unwrap();
    let _ = std::fs::remove_file(&path);
    // The child's CPU time is counted once it has been waited for; it ran
    // only inside the measured span (plus its start-up).
    println!(
        "{},{},{count},{:.0},{:.3},{:.3}",
        args[1],
        args[2],
        count as f64 / t,
        t * 1e6 / count as f64,
        c / count as f64
    );
}
