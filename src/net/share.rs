//! Which ports the sockets bound by this process hold, so that `bind` shares
//! a port between the workers of one process and with no one else.
//!
//! Every worker binds its own socket to the same port, which on Linux takes
//! `SO_REUSEPORT` on each of them. The kernel then lets any process of the
//! same user bind that port too and gives it a share of the traffic, so a
//! service started twice by mistake would come up twice and split its
//! clients. To refuse that, the first bind of a port in this process is
//! preceded by a probe: a socket without `SO_REUSEPORT` bound to the same
//! address and closed again, which fails with `EADDRINUSE` when anything
//! else holds the port. Later binds of the port (the other workers) skip the
//! probe, which their own process would fail.
//!
//! A socket that was dropped is closed through the ring, a moment later, and
//! would fail the probe of a bind that follows at once. So a dropped socket
//! is first marked ([`Held::release`]) in a way that takes it out of the
//! probe's sight: a TCP listener stops listening, and a UDP socket gets
//! `SO_REUSEADDR`, which the probe of a UDP port then sets too and live
//! sockets never do.

use std::collections::BTreeMap;
use std::io;
use std::net::{Shutdown, SocketAddr};
use std::sync::{Mutex, MutexGuard, PoisonError};

use socket2::{Domain, Protocol, Socket, Type};

/// The transport a port belongs to: TCP and UDP ports are separate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Proto {
    Tcp,
    Udp,
}

/// What this process did with one port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Port {
    /// Live sockets bound to it through [`bind`].
    live: usize,
    /// A socket bound to it was dropped, and may not be closed yet.
    released: bool,
}

/// The ports this process bound sockets to. At most one entry per port and
/// transport, kept once a socket was released.
static PORTS: Mutex<BTreeMap<(Proto, u16), Port>> = Mutex::new(BTreeMap::new());

fn ports() -> MutexGuard<'static, BTreeMap<(Proto, u16), Port>> {
    // The map is only counted in, so it is whole even after a panic.
    PORTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One socket's hold on a port, given up with [`release`](Self::release).
#[derive(Debug)]
pub(crate) struct Held {
    proto: Proto,
    port: u16,
}

impl Held {
    /// Gives the port up for `sock`, which is about to be closed: marks the
    /// socket so that the probe of a later bind does not see it (the close
    /// itself goes through the ring and takes a moment), then counts it out.
    pub(crate) fn release(self, sock: &Socket) {
        let mut ports = ports();
        // Errors are ignored: a socket that cannot be marked only makes a
        // bind that follows at once fail with `AddrInUse`.
        match self.proto {
            // A socket that no longer listens does not conflict with one
            // that sets `SO_REUSEADDR`, as the probe and every listener do.
            Proto::Tcp => drop(sock.shutdown(Shutdown::Both)),
            Proto::Udp => {
                drop(sock.set_reuse_address(true));
                // Until it is closed, the socket is still a member of the
                // port's `SO_REUSEPORT` group, and the kernel would hand it
                // a share of the datagrams meant for the sockets bound
                // after it. Connected to its own address, it matches no
                // other sender, so they all go to the live sockets.
                if let Some(local) = sock.local_addr().ok().and_then(|a| a.as_socket()) {
                    drop(sock.connect(&local.into()));
                }
            }
        }
        let port = ports.entry((self.proto, self.port)).or_default();
        port.live = port.live.saturating_sub(1);
        port.released = true;
    }
}

/// Binds a socket to `addr` with `bind`, which is refused with `AddrInUse`
/// when a socket of another process, or one this process did not bind
/// through here, holds the port. Port 0 is not probed: the kernel picks the
/// port.
///
/// `bind` creates the socket, sets its options and binds it; `share` says
/// whether the socket takes part in sharing at all (a UDP socket on port 0
/// does not: it is bound without `SO_REUSEPORT`).
pub(crate) fn bind(
    proto: Proto,
    addr: SocketAddr,
    share: bool,
    bind: impl FnOnce() -> io::Result<Socket>,
) -> io::Result<(Socket, Option<Held>)> {
    if !share {
        return Ok((bind()?, None));
    }
    // Held across the probe and the bind, so that two workers binding the
    // same port for the first time do not both probe, or probe each other.
    let mut ports = ports();
    if addr.port() != 0 {
        let known = ports
            .get(&(proto, addr.port()))
            .copied()
            .unwrap_or_default();
        if known.live == 0 {
            probe(proto, addr, known.released)?;
        }
    }
    let sock = bind()?;
    let port = match addr.port() {
        0 => sock
            .local_addr()?
            .as_socket()
            .map_or(0, |local| local.port()),
        port => port,
    };
    ports.entry((proto, port)).or_default().live += 1;
    Ok((sock, Some(Held { proto, port })))
}

/// Fails with `EADDRINUSE` when a socket that is not one of this process's
/// released ones holds `addr`. `released` says whether there may be such
/// sockets.
fn probe(proto: Proto, addr: SocketAddr, released: bool) -> io::Result<()> {
    let domain = Domain::for_address(addr);
    let sock = match proto {
        Proto::Tcp => {
            let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
            // What every listener sets: connections left over from an
            // earlier listener (in `TIME_WAIT`, or still being served) do
            // not keep the port; a socket that listens does, whatever its
            // options.
            sock.set_reuse_address(true)?;
            sock
        }
        Proto::Udp => {
            let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
            // Two UDP sockets that both set `SO_REUSEADDR` never conflict,
            // so this lets the probe pass the sockets `Held::release`
            // marked. It would also pass another process's socket that
            // sets it, so it is set only while this process may have
            // released sockets on the port.
            if released {
                sock.set_reuse_address(true)?;
            }
            sock
        }
    };
    sock.bind(&addr.into())?;
    check_bound(&sock, addr)
}

/// Checks that `bind` left the TCP socket on the port that was asked for,
/// and reports `EADDRINUSE` when it did not.
///
/// Linux 6.0.16 to 6.0.19, the last four releases of the 6.0 series, return
/// 1 instead of -1 from bind(2) on a TCP socket whose address is in use
/// (6.0.15 and older, 5.15 and 6.1 return -1 with `EADDRINUSE`, as do UDP
/// and Unix sockets on those four). The standard library and `socket2` test
/// for -1 like every caller of libc, so they take it for success. The socket
/// is still unbound, and listen(2) then gives it a free port on every
/// address: a server asked for a busy port would come up on another one,
/// open to all interfaces.
pub(crate) fn check_bound(sock: &Socket, asked: SocketAddr) -> io::Result<()> {
    let port = sock
        .local_addr()?
        .as_socket()
        .map_or(0, |local| local.port());
    if port == 0 || (asked.port() != 0 && port != asked.port()) {
        return Err(io::Error::from_raw_os_error(libc::EADDRINUSE));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A port that was free a moment ago.
    fn free_port() -> SocketAddr {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.local_addr().unwrap()
    }

    fn udp(addr: SocketAddr) -> io::Result<(Socket, Option<Held>)> {
        bind(Proto::Udp, addr, true, || {
            let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
            sock.set_reuse_port(true)?;
            sock.bind(&addr.into())?;
            Ok(sock)
        })
    }

    fn known(addr: SocketAddr) -> Port {
        ports()
            .get(&(Proto::Udp, addr.port()))
            .copied()
            .unwrap_or_default()
    }

    #[test]
    fn a_released_port_is_counted_out_and_can_be_bound_again() {
        let addr = free_port();
        let (first, hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 1);
        let (second, second_hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 2);

        hold.unwrap().release(&first);
        assert_eq!(known(addr).live, 1);
        second_hold.unwrap().release(&second);
        assert_eq!(
            known(addr),
            Port {
                live: 0,
                released: true
            }
        );

        // Both sockets are still open, and marked: a new bind passes them.
        let (third, third_hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 1);
        third_hold.unwrap().release(&third);
    }
}
