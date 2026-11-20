//! Binding a port again right after dropping its socket, while other threads
//! read /proc/self/fd, as `lsof`, `ss -p`, monitoring agents and a count of
//! open descriptors do.
//!
//! Listing or reading those links takes a reference to each open file for a
//! moment. A socket closed in that moment is only taken off its port when
//! the reading thread returns to user space, microseconds later. A bind that
//! closed a socket of its own just before binding (as the probe of the first
//! bind of a port once did) was then refused with `AddrInUse` for a port
//! that nothing held.
//!
//! In a test binary of its own: the readers would hold the sockets of every
//! other test too.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use giong::net::{TcpListener, UdpSocket};

/// Threads that list and read /proc/self/fd until dropped.
struct Readers {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Readers {
    fn start(n: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let threads = (0..n)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let Ok(dir) = std::fs::read_dir("/proc/self/fd") else {
                            continue;
                        };
                        for entry in dir.flatten() {
                            drop(std::fs::read_link(entry.path()));
                        }
                    }
                })
            })
            .collect();
        Self { stop, threads }
    }
}

impl Drop for Readers {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            drop(thread.join());
        }
    }
}

/// A port no socket held a moment ago, found before the readers start.
fn free_addr() -> SocketAddr {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = sock.local_addr().unwrap();
    let tcp = std::net::TcpListener::bind(addr);
    drop((sock, tcp));
    addr
}

const RUN_FOR: Duration = Duration::from_secs(2);

#[test]
fn a_udp_port_can_be_bound_again_at_once_while_proc_is_read() {
    let addr = free_addr();
    let readers = Readers::start(2);
    giong::block_on(async move {
        let started = Instant::now();
        let mut binds = 0u64;
        while started.elapsed() < RUN_FOR {
            // The first bind of the port in this process, then a second one
            // that shares it, as every worker does.
            let first = UdpSocket::bind(addr)
                .unwrap_or_else(|e| panic!("bind {binds} of {addr} failed: {e}"));
            let second = UdpSocket::bind(addr)
                .unwrap_or_else(|e| panic!("shared bind {binds} of {addr} failed: {e}"));
            drop((first, second));
            binds += 2;
        }
        eprintln!("{binds} binds of {addr}");
    });
    drop(readers);
}

#[test]
fn a_tcp_port_can_be_bound_again_at_once_while_proc_is_read() {
    let addr = free_addr();
    let readers = Readers::start(2);
    giong::block_on(async move {
        let started = Instant::now();
        let mut binds = 0u64;
        while started.elapsed() < RUN_FOR {
            let first = TcpListener::bind(addr)
                .unwrap_or_else(|e| panic!("bind {binds} of {addr} failed: {e}"));
            let second = TcpListener::bind(addr)
                .unwrap_or_else(|e| panic!("shared bind {binds} of {addr} failed: {e}"));
            drop((first, second));
            binds += 2;
        }
        eprintln!("{binds} binds of {addr}");
    });
    drop(readers);
}
