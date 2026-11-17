//! How much of a small-packet server's CPU is the kernel's network stack, and
//! how much an AF_XDP path (frames handed straight from the driver to user
//! space) saves. A UDP echo server on one pinned core, four ways:
//!
//!   kernel   recvmmsg/sendmmsg, 32 per call: the cheapest a socket gets
//!   giong    giong::net::UdpSocket on one worker (16 echo tasks)
//!   tokio    tokio::net::UdpSocket on a current-thread runtime (16 tasks)
//!   xdp      AF_XDP socket: an XDP program redirects every frame of the
//!            interface into a user-space ring; the server swaps addresses
//!            and sends the same frame back through the TX ring
//!
//! The client runs in another network namespace on the far end of a veth
//! pair and keeps a window of packets in flight per thread. Reported: echoes
//! per second, and CPU time of the whole machine per echo (every CPU, user +
//! system + softirq), because on veth part of the receive work runs on the
//! sender's CPU. The client's own cost is the same in every mode, so the
//! difference between modes is the server side.
//!
//!     bench-xdp server <mode> <iface> <cpu>        (root namespace)
//!     bench-xdp client <dst ip> <threads> <secs>    (client namespace)
//!
//! bench/xdp.sh sets up the veth pair and runs the rounds.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PORT: u16 = 7777;
const PAYLOAD: usize = 64;
const BATCH: usize = 32;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("server") => {
            let cpu: usize = args[4].parse().unwrap();
            pin(cpu);
            match args[2].as_str() {
                "kernel" => kernel_server(),
                "giong" => giong_server(),
                "giong-epoll" => {
                    std::env::set_var("GIONG_DRIVER", "epoll");
                    giong_server()
                }
                "tokio" => tokio_server(),
                "xdp" => xdp::server(&args[3]),
                m => panic!("unknown mode {m}"),
            }
        }
        Some("client") => client(
            args[2].parse().unwrap(),
            args[3].parse().unwrap(),
            args[4].parse().unwrap(),
        ),
        _ => {
            eprintln!("usage: bench-xdp server <mode> <iface> <cpu> | client <ip> <threads> <secs>")
        }
    }
}

fn pin(cpu: usize) {
    // SAFETY: plain syscall with a zeroed, then filled, cpu_set_t.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

/// Non-idle jiffies of the whole machine (user+nice+system+irq+softirq+steal).
fn busy_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/stat").unwrap();
    let f: Vec<u64> = stat
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .skip(1)
        .map(|x| x.parse().unwrap())
        .collect();
    f[0] + f[1] + f[2] + f[5] + f[6] + f[7]
}

fn client(dst: Ipv4Addr, threads: usize, secs: u64) {
    let stop = Arc::new(AtomicBool::new(false));
    let echoes = Arc::new(AtomicU64::new(0));
    let hz = 100.0; // USER_HZ
    let handles: Vec<_> = (0..threads)
        .map(|i| {
            let (stop, echoes) = (stop.clone(), echoes.clone());
            std::thread::spawn(move || {
                let s = UdpSocket::bind(("0.0.0.0", 0)).unwrap();
                s.connect((dst, PORT)).unwrap();
                s.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
                let fd = std::os::fd::AsRawFd::as_raw_fd(&s);
                let _ = i;
                let mut bufs = vec![[0u8; 2048]; BATCH];
                let payload = [0x5au8; PAYLOAD];
                let mut window = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    // Keep 4 batches in flight.
                    while window < 4 * BATCH {
                        sendmmsg(fd, &payload, BATCH);
                        window += BATCH;
                    }
                    let n = recvmmsg(fd, &mut bufs);
                    if n == 0 {
                        window = 0; // lost some: start the window again
                        continue;
                    }
                    window = window.saturating_sub(n);
                    echoes.fetch_add(n as u64, Ordering::Relaxed);
                }
            })
        })
        .collect();
    std::thread::sleep(Duration::from_secs(1));
    let (e0, t0, w0) = (echoes.load(Ordering::Relaxed), busy_ticks(), Instant::now());
    std::thread::sleep(Duration::from_secs(secs));
    let (e1, t1, el) = (
        echoes.load(Ordering::Relaxed),
        busy_ticks(),
        w0.elapsed().as_secs_f64(),
    );
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    let n = (e1 - e0) as f64;
    let cpu_s = (t1 - t0) as f64 / hz;
    println!("{:.0},{:.3}", n / el, cpu_s / n * 1e6);
}

fn sendmmsg(fd: i32, payload: &[u8], n: usize) {
    let mut iov: Vec<libc::iovec> = (0..n)
        .map(|_| libc::iovec {
            iov_base: payload.as_ptr() as *mut _,
            iov_len: payload.len(),
        })
        .collect();
    // SAFETY: zeroed mmsghdr with pointers into `iov`, which outlives the call.
    let mut msgs: Vec<libc::mmsghdr> = (0..n)
        .map(|i| {
            let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
            m.msg_hdr.msg_iov = &mut iov[i];
            m.msg_hdr.msg_iovlen = 1;
            m
        })
        .collect();
    // SAFETY: valid vector of n headers.
    unsafe { libc::sendmmsg(fd, msgs.as_mut_ptr(), n as u32, 0) };
}

fn recvmmsg(fd: i32, bufs: &mut [[u8; 2048]]) -> usize {
    let mut iov: Vec<libc::iovec> = bufs
        .iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.as_mut_ptr() as *mut _,
            iov_len: b.len(),
        })
        .collect();
    let mut msgs: Vec<libc::mmsghdr> = (0..bufs.len())
        .map(|i| {
            // SAFETY: plain C struct, zero is valid.
            let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
            m.msg_hdr.msg_iov = &mut iov[i];
            m.msg_hdr.msg_iovlen = 1;
            m
        })
        .collect();
    // SAFETY: valid buffers; MSG_WAITFORONE returns after the first datagram.
    let n = unsafe {
        libc::recvmmsg(
            fd,
            msgs.as_mut_ptr(),
            bufs.len() as u32,
            libc::MSG_WAITFORONE,
            std::ptr::null_mut(),
        )
    };
    n.max(0) as usize
}

fn kernel_server() {
    let s = UdpSocket::bind(("0.0.0.0", PORT)).unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&s);
    let mut bufs = vec![[0u8; 2048]; BATCH];
    let mut names: Vec<libc::sockaddr_in> = vec![unsafe { std::mem::zeroed() }; BATCH];
    let mut iov: Vec<libc::iovec> = bufs
        .iter_mut()
        .map(|b| libc::iovec {
            iov_base: b.as_mut_ptr() as *mut _,
            iov_len: b.len(),
        })
        .collect();
    let mut msgs: Vec<libc::mmsghdr> = (0..BATCH)
        .map(|i| {
            // SAFETY: plain C struct.
            let mut m: libc::mmsghdr = unsafe { std::mem::zeroed() };
            m.msg_hdr.msg_iov = &mut iov[i];
            m.msg_hdr.msg_iovlen = 1;
            m.msg_hdr.msg_name = &mut names[i] as *mut _ as *mut _;
            m.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as u32;
            m
        })
        .collect();
    loop {
        for m in msgs.iter_mut() {
            m.msg_hdr.msg_namelen = std::mem::size_of::<libc::sockaddr_in>() as u32;
        }
        for (i, m) in msgs.iter_mut().enumerate() {
            iov[i].iov_len = 2048;
            m.msg_hdr.msg_iov = &mut iov[i];
        }
        // SAFETY: buffers and names live for the loop.
        let n = unsafe {
            libc::recvmmsg(
                fd,
                msgs.as_mut_ptr(),
                BATCH as u32,
                libc::MSG_WAITFORONE,
                std::ptr::null_mut(),
            )
        };
        if n <= 0 {
            continue;
        }
        for i in 0..n as usize {
            iov[i].iov_len = msgs[i].msg_len as usize;
        }
        // SAFETY: as above.
        unsafe { libc::sendmmsg(fd, msgs.as_mut_ptr(), n as u32, 0) };
    }
}

/// Echo tasks per server (`TASKS`, 16 by default), all on one socket.
fn tasks() -> usize {
    std::env::var("TASKS").ok().and_then(|t| t.parse().ok()).unwrap_or(16)
}

fn giong_server() {
    giong::Builder::new()
        .workers(1)
        .pin_cores(false)
        .build()
        .unwrap()
        .run(|_| async {
            let s = std::rc::Rc::new(giong::net::UdpSocket::bind(("0.0.0.0", PORT)).unwrap());
            for _ in 1..tasks() {
                let s = s.clone();
                giong::spawn(async move { giong_echo(&s).await });
            }
            giong_echo(&s).await;
        })
        .unwrap();
}

async fn giong_echo(s: &giong::net::UdpSocket) {
    // One buffer per task, handed to each send and back, as Tokio's tasks
    // keep theirs.
    let mut v = Vec::with_capacity(2048);
    loop {
        let Ok((d, from)) = s.recv_from().await else {
            continue;
        };
        v.clear();
        v.extend_from_slice(&d);
        drop(d);
        v = s.send_to(v, from).await.1;
    }
}

fn tokio_server() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    rt.block_on(async {
        let s = Arc::new(
            tokio::net::UdpSocket::bind(("0.0.0.0", PORT))
                .await
                .unwrap(),
        );
        let mut handles = Vec::new();
        for _ in 0..tasks() {
            let s = s.clone();
            handles.push(tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                loop {
                    let Ok((n, from)) = s.recv_from(&mut buf).await else {
                        continue;
                    };
                    let _ = s.send_to(&buf[..n], from).await;
                }
            }));
        }
        for t in handles {
            let _ = t.await;
        }
    });
}

mod xdp;
