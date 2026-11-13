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
    (socket, addr)
}

/// A port on 127.0.0.1 for listeners bound with `SO_REUSEPORT` (as
/// `TcpListener::bind` does) to share, held by the returned socket.
///
/// The socket is bound with the same options but does not listen, so it gets
/// no connections, and no socket of another test can take the port before
/// the listeners have bound it. Keep it until they have.
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
