//! Which ports the sockets bound by this process hold, so that `bind` shares
//! a port between the workers of one process and with no one else.
//!
//! Every worker binds its own socket to the same port, which on Linux takes
//! `SO_REUSEPORT` on each of them. The kernel then lets any process of the
//! same user bind that port too and gives it a share of the traffic, so a
//! service started twice by mistake would come up twice and split its
//! clients. To refuse that, the first socket of a port in this process is
//! bound without `SO_REUSEPORT`, which fails with `EADDRINUSE` when anything
//! else holds the port, and only takes `SO_REUSEPORT` once it is bound.
//! Later binds of the port (the other workers) set it before binding, and
//! join the first.
//!
//! The check is the bind of the socket that stays, not of a separate probe
//! socket closed again before it: a socket that was just closed can keep its
//! port for a moment. While any thread reads /proc/self/fd (`lsof`, `ss -p`,
//! a monitoring agent, a count of open descriptors), each open file is held
//! for an instant, and a socket closed in that instant leaves its port only
//! when the reading thread returns to user space. A bind right after the
//! probe's close was then refused for a port nothing held.
//!
//! A socket that was dropped is closed through the ring, a moment later, and
//! would fail the first bind of a port that follows at once. So a dropped
//! socket is first marked ([`Held::release`]) in a way that takes it out of
//! that bind's sight: a TCP listener stops listening, and a UDP socket gets
//! `SO_REUSEADDR`, which the first bind of a UDP port then sets too, for the
//! bind only. Live sockets never keep it.

use std::collections::BTreeMap;
use std::io;
use std::net::{Shutdown, SocketAddr};
use std::sync::{Mutex, MutexGuard, PoisonError};

use socket2::Socket;

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
    /// socket so that the first bind of the port that follows does not see
    /// it (the close itself goes through the ring and takes a moment), then
    /// counts it out.
    pub(crate) fn release(self, sock: &Socket) {
        let mut ports = ports();
        // Errors are ignored: a socket that cannot be marked only makes a
        // bind that follows at once fail with `AddrInUse`.
        match self.proto {
            // A socket that no longer listens does not conflict with one
            // that sets `SO_REUSEADDR`, as every listener does.
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

/// Creates a socket with `new` and binds it to `addr`, refused with
/// `AddrInUse` when a socket of another process, or one this process did
/// not bind through here, holds the port. Port 0 is not checked: the kernel
/// picks the port.
///
/// `new` creates the socket and sets its other options; this sets
/// `SO_REUSEADDR` and `SO_REUSEPORT` and binds. `share` says whether the
/// socket takes part in sharing at all (a UDP socket on port 0 does not: it
/// is bound without `SO_REUSEPORT`).
pub(crate) fn bind(
    proto: Proto,
    addr: SocketAddr,
    share: bool,
    new: impl FnOnce() -> io::Result<Socket>,
) -> io::Result<(Socket, Option<Held>)> {
    let sock = new()?;
    // What every listener sets: connections left over from an earlier
    // listener (in `TIME_WAIT`, or still being served) do not keep the port;
    // a socket that listens does, whatever its options. A UDP socket keeps
    // it off: with `SO_REUSEADDR`, any user's socket that sets it too could
    // bind the port and receive its datagrams. `SO_REUSEPORT` is for sockets
    // of the same user.
    if proto == Proto::Tcp {
        sock.set_reuse_address(true)?;
    }
    if !share {
        sock.bind(&addr.into())?;
        check_bound(&sock, addr)?;
        return Ok((sock, None));
    }
    // Held across the bind, so that two workers binding the same port for
    // the first time do not both bind it alone, or refuse each other.
    let mut ports = ports();
    let known = ports
        .get(&(proto, addr.port()))
        .copied()
        .unwrap_or_default();
    // The first bind without `SO_REUSEPORT` relies on Linux's rules: there
    // the option can be set on a bound socket and later binds join it. On
    // macOS every socket of a port needs it before it binds, so the port
    // is not guarded against another process there (see
    // docs/design/portability.md).
    if cfg!(target_os = "linux") && addr.port() != 0 && known.live == 0 {
        bind_first(proto, &sock, addr, known.released)?;
    } else {
        sock.set_reuse_port(true)?;
        sock.bind(&addr.into())?;
        check_bound(&sock, addr)?;
    }
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

/// Binds the first socket of a port in this process: without
/// `SO_REUSEPORT`, so that the bind fails with `EADDRINUSE` when a socket
/// that is not one of this process's released ones holds `addr`, then sets
/// it for the workers that join. `released` says whether there may be such
/// sockets.
fn bind_first(proto: Proto, sock: &Socket, addr: SocketAddr, released: bool) -> io::Result<()> {
    // Two UDP sockets that both set `SO_REUSEADDR` never conflict, so this
    // lets the bind pass the sockets `Held::release` marked. It would also
    // pass another process's socket that sets it, so it is set only while
    // this process may have released sockets on the port, and only for the
    // bind.
    let marked = proto == Proto::Udp && released;
    if marked {
        sock.set_reuse_address(true)?;
    }
    sock.bind(&addr.into())?;
    check_bound(sock, addr)?;
    // The kernel looks at both options as they are when a later socket
    // binds, so the next worker joins this one.
    sock.set_reuse_port(true)?;
    if marked {
        sock.set_reuse_address(false)?;
    }
    Ok(())
}

/// Checks that `bind` left the socket on the port that was asked for, and
/// reports `EADDRINUSE` when it did not.
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
        return Err(io::Error::from_raw_os_error(crate::sys::errno::EADDRINUSE));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::{Domain, Protocol, Type};

    /// A port that was free a moment ago.
    fn free_port() -> SocketAddr {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.local_addr().unwrap()
    }

    fn udp(addr: SocketAddr) -> io::Result<(Socket, Option<Held>)> {
        bind(Proto::Udp, addr, true, || {
            Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))
        })
    }

    fn known(addr: SocketAddr) -> Port {
        ports()
            .get(&(Proto::Udp, addr.port()))
            .copied()
            .unwrap_or_default()
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot set SO_REUSEPORT")]
    fn the_first_socket_of_a_port_refuses_another_holder_and_joins_the_workers() {
        let addr = free_port();
        // Another process's socket, as far as this module knows.
        let other = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        other.set_reuse_port(true).unwrap();
        other.bind(&addr.into()).unwrap();
        let err = udp(addr).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(known(addr).live, 0);
        drop(other);

        let (first, hold) = udp(addr).unwrap();
        assert!(!first.reuse_address().unwrap());
        assert!(first.reuse_port().unwrap());
        let (second, second_hold) = udp(addr).unwrap();

        // The two split the datagrams of many senders between them, by a
        // hash of the sender's address.
        for _ in 0..64 {
            let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            sender.send_to(b"x", addr).unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        let count = |sock: &Socket| {
            sock.set_nonblocking(true).unwrap();
            let mut buf = [std::mem::MaybeUninit::new(0u8); 8];
            std::iter::from_fn(|| sock.recv(&mut buf).ok()).count()
        };
        let (a, b) = (count(&first), count(&second));
        assert_eq!(a + b, 64);
        assert!(
            a > 0 && b > 0,
            "one socket got all 64 datagrams: {a} and {b}"
        );

        second_hold.unwrap().release(&second);
        hold.unwrap().release(&first);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot set SO_REUSEPORT")]
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
        // It sets `SO_REUSEADDR` for the bind only, and joins the port's
        // `SO_REUSEPORT` group once bound.
        let (third, third_hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 1);
        assert!(!third.reuse_address().unwrap());
        assert!(third.reuse_port().unwrap());
        let (fourth, fourth_hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 2);
        fourth_hold.unwrap().release(&fourth);
        third_hold.unwrap().release(&third);
    }
}
