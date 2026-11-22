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
//! other test too. Linux only: `/proc` is its interface.
#![cfg(target_os = "linux")]

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

/// Binds the first free port of `ports` with `bind`, starting at a random
/// one, and returns its address with the socket.
///
/// The bind under test is itself what finds the port. Binding a socket of
/// the standard library to port 0 and closing it would not do: that port is
/// in the range the kernel hands out to every connection of the machine, so
/// another process can hold it in TCP, and the closed socket itself keeps
/// the port for a moment when the readers of the other test are running
/// (both failed the first bind on CI). `ports` lies below that range, and
/// each test has its own.
fn bind_free<T>(
    ports: std::ops::Range<u16>,
    bind: impl Fn(SocketAddr) -> std::io::Result<T>,
) -> (SocketAddr, T) {
    use std::hash::{BuildHasher, Hasher};
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let len = u64::from(ports.end - ports.start);
    for i in 0..len {
        let port = ports.start + ((random + i) % len) as u16;
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        match bind(addr) {
            Ok(socket) => return (addr, socket),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {}
            Err(e) => panic!("bind of {addr} failed: {e}"),
        }
    }
    panic!("no free port in {ports:?}");
}

/// Where the ports the kernel hands out by itself begin.
fn ephemeral_start() -> u16 {
    let range = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").unwrap();
    let low: u16 = range.split_whitespace().next().unwrap().parse().unwrap();
    assert!(low > 12_000, "no room below the ephemeral range at {low}");
    low
}

const RUN_FOR: Duration = Duration::from_secs(2);

#[test]
fn a_udp_port_can_be_bound_again_at_once_while_proc_is_read() {
    let ports = 10_000..10_000 + (ephemeral_start() - 10_000) / 2;
    let readers = Readers::start(2);
    giong::block_on(async move {
        let (addr, found) = bind_free(ports, UdpSocket::bind);
        drop(found);
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
    let ports = 10_000 + (ephemeral_start() - 10_000) / 2..ephemeral_start();
    let readers = Readers::start(2);
    giong::block_on(async move {
        let (addr, found) = bind_free(ports, TcpListener::bind);
        drop(found);
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
