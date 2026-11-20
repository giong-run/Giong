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
#[cfg(unix)]
pub fn shared_port() -> (Socket, u16) {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket.set_reuse_address(true).unwrap();
    socket.set_reuse_port(true).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let port = socket.local_addr().unwrap().as_socket().unwrap().port();
    (socket, port)
}

/// A port on 127.0.0.1 for the listeners of a test to share. Windows has
/// no `SO_REUSEPORT` to hold it with: the port is one that was free a
/// moment ago, and the short race with another bind is accepted.
#[cfg(windows)]
pub fn shared_port() -> (Socket, u16) {
    let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    let port = socket.local_addr().unwrap().as_socket().unwrap().port();
    drop(socket);
    (Socket::new(Domain::IPV4, Type::STREAM, None).unwrap(), port)
}

/// An address on 127.0.0.1 where a UDP datagram is refused (the kernel
/// answers with "port unreachable"), for as long as the returned socket
/// lives.
///
/// The socket holds the port, so no other test can bind it, and is
/// connected elsewhere: a connected UDP socket only takes datagrams from its
/// peer, so for anyone else nothing listens on the port.
pub fn refusing_udp_addr() -> (Socket, SocketAddr) {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
    socket
        .bind(&SocketAddr::from(([127, 0, 0, 1], 0)).into())
        .unwrap();
    socket
        .connect(&SocketAddr::from(([127, 0, 0, 1], 9)).into())
        .unwrap();
    let addr = socket.local_addr().unwrap().as_socket().unwrap();
    // Windows drops a datagram that a connected socket does not take
    // without telling the sender, so there the port is given back: a
    // closed port answers, and the short race with another bind is
    // accepted.
    #[cfg(windows)]
    let socket = {
        drop(socket);
        Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap()
    };
    (socket, addr)
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
