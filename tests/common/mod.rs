//! Helpers shared by the integration tests.

// Each test file compiles this module on its own and uses only part of it.
#![allow(dead_code)]

use std::net::SocketAddr;

use socket2::{Domain, Socket, Type};

/// An address on 127.0.0.1 where a TCP connect is refused, for as long as
/// the returned socket lives.
///
/// The socket is bound but does not listen. A port taken from a listener
/// that was closed at once is not refused for certain: a test running at
/// the same time can listen on it, or a connect to it can pick it as its
/// own source port and connect to itself.
pub fn refusing_addr() -> (Socket, SocketAddr) {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let addr = socket.local_addr().unwrap().as_socket().unwrap();
    // macOS drops a SYN to a socket that is bound but not listening (Linux
    // answers with a reset), so there the port is given back: a closed port
    // is refused, and the short race with another bind is accepted.
    #[cfg(target_os = "macos")]
    let socket = {
        drop(socket);
        Socket::new(Domain::IPV4, Type::STREAM, None).unwrap()
    };
    (socket, addr)
}

/// A port on 127.0.0.1 for listeners bound with `SO_REUSEPORT` (as
/// `TcpListener::bind` does) to share, held by the returned socket.
///
/// The socket is bound with the same options but does not listen, so it gets
/// no connections, and no socket of another test can take the port before
/// the listeners have bound it. Keep it until they have.
///
/// On macOS the listeners of a process share one socket, bound without
/// `SO_REUSEPORT`, which a holder would keep from binding: there the port is
/// given back at once (the returned socket holds nothing), and the short
/// race with another bind is accepted.
#[cfg(unix)]
pub fn shared_port() -> (Socket, u16) {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket.set_reuse_address(true).unwrap();
    socket.set_reuse_port(true).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let port = socket.local_addr().unwrap().as_socket().unwrap().port();
    #[cfg(target_os = "macos")]
    let socket = {
        drop(socket);
        Socket::new(Domain::IPV4, Type::STREAM, None).unwrap()
    };
    (socket, port)
}

/// A port on 127.0.0.1 for the listeners of a test to share.
///
/// Windows has no `SO_REUSEPORT` to hold it with, so the port is one that
/// is free now, and each call gives another one. A port from a bind to port
/// 0 that was given back at once is not enough: Windows can give the same
/// one to a test running beside this one, whose listeners then join this
/// test's (the listeners of a process share an address), and each test
/// gets some of the other's connections and waits for the rest. That hung
/// the balance tests once in about a hundred runs. The ports are taken
/// below the range Windows hands out for binds to port 0 (49152 and up),
/// and above the ones `refusing_udp_addr` takes.
#[cfg(windows)]
pub fn shared_port() -> (Socket, u16) {
    use std::sync::atomic::{AtomicU16, Ordering};
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let start = 20_000 + (std::process::id() as u16 % 1000) * 20 + 4_500;
    loop {
        // At most 39,980 + 9,000, below 49,152.
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        assert!(n < 4_500, "no free port below the ephemeral range");
        let addr = SocketAddr::from(([127, 0, 0, 1], start + n));
        let free = [Type::STREAM, Type::DGRAM].into_iter().all(|kind| {
            let probe = Socket::new(Domain::IPV4, kind, None).unwrap();
            probe.bind(&addr.into()).is_ok()
        });
        if free {
            return (
                Socket::new(Domain::IPV4, Type::STREAM, None).unwrap(),
                start + n,
            );
        }
    }
}

/// An address on 127.0.0.1 where a UDP datagram is refused (the kernel
/// answers with "port unreachable"), for as long as the returned socket
/// lives.
///
/// The socket holds the port, so no other test can bind it, and is
/// connected elsewhere: a connected UDP socket only takes datagrams from its
/// peer, so for anyone else nothing listens on the port.
#[cfg(unix)]
pub fn refusing_udp_addr() -> (Socket, SocketAddr) {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    socket
        .connect(&SocketAddr::from(([127, 0, 0, 1], 9)).into())
        .unwrap();
    let addr = socket.local_addr().unwrap().as_socket().unwrap();
    (socket, addr)
}

/// An address on 127.0.0.1 where a UDP datagram is refused (the kernel
/// answers with "port unreachable").
///
/// Windows drops a datagram that a connected socket does not take without
/// telling the sender, so the port cannot be held: it is a closed one. It
/// is taken below the range Windows hands out for binds to port 0
/// (49152 and up), so that no socket of a test running beside this one is
/// given it in the meantime; one given it after a datagram to a closed
/// ephemeral port went unanswered on a runner.
///
/// The returned socket holds the same port number for TCP, which leaves
/// UDP datagrams to it refused, and claims the port from other test
/// processes: their UDP probe of it (a bind, closed again) would take a
/// datagram sent there in the meantime, without "port unreachable". Runs
/// of 40 test processes started at once caught that.
#[cfg(windows)]
pub fn refusing_udp_addr() -> (Socket, SocketAddr) {
    let start = 20_000 + (std::process::id() as u16 % 1000) * 20;
    // Below the ports `shared_port` takes.
    (0..4_500u16)
        .map(|i| SocketAddr::from(([127, 0, 0, 1], start + i)))
        .find_map(|addr| {
            let claim = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
            claim.bind(&addr.into()).ok()?;
            let probe = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
            probe.bind(&addr.into()).ok()?;
            Some((claim, addr))
        })
        .expect("no free UDP port below the ephemeral range")
}

/// Error numbers the tests compare against, as each system spells them.
pub mod errno {
    /// A datagram too long to send.
    #[cfg(unix)]
    pub const EMSGSIZE: i32 = libc::EMSGSIZE;
    /// A datagram too long to send (`WSAEMSGSIZE`).
    #[cfg(windows)]
    pub const EMSGSIZE: i32 = 10040;

    /// A send without an address on a socket that has no peer.
    #[cfg(unix)]
    pub const NO_PEER: i32 = libc::EDESTADDRREQ;
    /// A send without an address on a socket that has no peer
    /// (`WSAENOTCONN`).
    #[cfg(windows)]
    pub const NO_PEER: i32 = 10057;
}
