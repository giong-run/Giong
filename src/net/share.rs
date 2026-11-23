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
//! Linux looks at `SO_REUSEPORT` as it is when a later socket binds, so the
//! first socket can take it after its own bind. gVisor's network stack
//! records it at the bind: there the socket whose bind made the check is
//! closed, and the one that stays is bound with `SO_REUSEPORT` from the start
//! ([`bind_first`]).
//!
//! On Linux the check is the bind of the socket that stays, not of a separate
//! probe socket closed again before it: a socket that was just closed can
//! keep its port for a moment. While any thread reads /proc/self/fd (`lsof`, `ss -p`,
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
//!
//! # macOS
//!
//! `SO_REUSEPORT` means something else there: every socket of a port needs
//! it before it binds, connections and datagrams then go to one of the
//! sockets (the last one bound), not to all of them, and a socket bound to
//! the wildcard address with it lets any user's socket that sets it too
//! bind the port and take its traffic. So on macOS no socket gets it: the
//! first socket of an address in this process binds without it, which
//! refuses a port anything else holds and keeps every other process out
//! while it lives. The workers that bind the same address afterwards get a
//! duplicate of that one socket ([`Socket::try_clone`]): one queue of
//! connections or datagrams, taken from by every worker that waits on it.
//! The socket goes away when its last duplicate is dropped. For UDP this
//! means that [`connect`](crate::net::UdpSocket::connect) on one of them
//! connects them all.
//!
//! The Linux test build of the kqueue driver (`--cfg giong_libkqueue`) uses
//! the same code, so that it runs without a Mac.

use std::collections::BTreeMap;
use std::io;
#[cfg(unix)]
use std::net::Ipv4Addr;
use std::net::{Shutdown, SocketAddr};
#[cfg(any(target_os = "macos", giong_libkqueue))]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::OnceLock;
use std::sync::{Mutex, MutexGuard, PoisonError};

use socket2::Socket;
#[cfg(unix)]
use socket2::{Domain, Type};

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

/// The sockets the workers share on macOS, by transport and the address
/// they were bound to: the map's own duplicate of the socket, which later binds of
/// the address duplicate again. The `Arc` counts the sockets that use it
/// (each holds one in its [`Held`]); the entry is removed with the last of
/// them. Only touched with [`PORTS`] locked.
#[cfg(any(target_os = "macos", giong_libkqueue))]
static SHARED: Mutex<BTreeMap<(Proto, SocketAddr), Arc<Socket>>> = Mutex::new(BTreeMap::new());

#[cfg(any(target_os = "macos", giong_libkqueue))]
fn shared_sockets() -> MutexGuard<'static, BTreeMap<(Proto, SocketAddr), Arc<Socket>>> {
    // An entry is inserted or removed in one step, so the map is whole even
    // after a panic.
    SHARED.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One socket's hold on a port, given up with [`release`](Self::release).
#[derive(Debug)]
pub(crate) struct Held {
    proto: Proto,
    port: u16,
    /// The shared socket this one is a duplicate of, and its address.
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    shared: Option<(SocketAddr, Arc<Socket>)>,
}

impl Held {
    /// Gives the port up for `sock`, which is about to be closed: marks the
    /// socket so that the first bind of the port that follows does not see
    /// it (the close itself goes through the ring and takes a moment), then
    /// counts it out.
    pub(crate) fn release(self, sock: &Socket) {
        let mut ports = ports();
        #[cfg(any(target_os = "macos", giong_libkqueue))]
        if let Some((addr, shared)) = self.shared {
            let mut map = shared_sockets();
            // The map's own count and this one: the last socket of the
            // address. The others keep using it, so it is left as it is for
            // them.
            if Arc::strong_count(&shared) == 2 {
                map.remove(&(self.proto, addr));
                // Where a socket can stop listening (Linux), it does, as
                // below; on macOS this fails and the close frees the port.
                if self.proto == Proto::Tcp {
                    drop(sock.shutdown(Shutdown::Both));
                }
            }
            let port = ports.entry((self.proto, self.port)).or_default();
            port.live = port.live.saturating_sub(1);
            return;
        }
        // Errors are ignored: a socket that cannot be marked only makes a
        // bind that follows at once fail with `AddrInUse`.
        match self.proto {
            // A socket that no longer listens does not conflict with one
            // that sets `SO_REUSEADDR`, as every listener does.
            Proto::Tcp => drop(sock.shutdown(Shutdown::Both)),
            // Windows closes a socket at once, and its sockets are bound
            // exclusively, which the option cannot be set on.
            Proto::Udp if cfg!(windows) => {}
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
/// is bound without `SO_REUSEPORT`). `new` may be called twice, where the
/// first socket of a port has to be bound again (see [`bind_first`]).
pub(crate) fn bind(
    proto: Proto,
    addr: SocketAddr,
    share: bool,
    new: impl Fn() -> io::Result<Socket>,
) -> io::Result<(Socket, Option<Held>)> {
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    if share {
        return bind_shared(proto, addr, new);
    }
    bind_with(proto, addr, share, new, Joins::Find)
}

/// How the first socket of a port learns whether sockets bound later can
/// join it when it takes `SO_REUSEPORT` after its own bind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Joins {
    /// Find out ([`late_reuse_port_joins`]).
    Find,
    /// They cannot, as on gVisor. For tests.
    #[cfg(all(test, unix))]
    Never,
}

fn bind_with(
    proto: Proto,
    addr: SocketAddr,
    share: bool,
    new: impl Fn() -> io::Result<Socket>,
    joins: Joins,
) -> io::Result<(Socket, Option<Held>)> {
    let new = || {
        let sock = new()?;
        // What every listener sets: connections left over from an earlier
        // listener (in `TIME_WAIT`, or still being served) do not keep the
        // port; a socket that listens does, whatever its options. A UDP
        // socket keeps it off: with `SO_REUSEADDR`, any user's socket that
        // sets it too could bind the port and receive its datagrams.
        // `SO_REUSEPORT` is for sockets of the same user.
        //
        // Not on Windows: `SO_REUSEADDR` there lets a second socket bind a
        // port that is being listened on and take its connections, and a
        // closed listener's port is free again at once without it.
        if proto == Proto::Tcp && cfg!(unix) {
            sock.set_reuse_address(true)?;
        }
        // Nor can another socket that sets `SO_REUSEADDR` bind over this one
        // there (a more specific address on the same port, which would take
        // its connections or datagrams): Windows refuses that only for a
        // socket bound with `SO_EXCLUSIVEADDRUSE`.
        #[cfg(windows)]
        crate::sys::set_exclusive_address(&sock)?;
        Ok::<_, io::Error>(sock)
    };
    if !share {
        let sock = new()?;
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
    let sock = if addr.port() != 0 && known.live == 0 {
        bind_first(proto, new, addr, known.released, joins)?
    } else {
        let sock = new()?;
        set_reuse_port(&sock)?;
        sock.bind(&addr.into())?;
        check_bound(&sock, addr)?;
        sock
    };
    let port = match addr.port() {
        0 => sock
            .local_addr()?
            .as_socket()
            .map_or(0, |local| local.port()),
        port => port,
    };
    ports.entry((proto, port)).or_default().live += 1;
    Ok((
        sock,
        Some(Held {
            proto,
            port,
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            shared: None,
        }),
    ))
}

/// The macOS way to share a port between workers: the first socket of
/// `addr` binds a socket made by `new`, without `SO_REUSEPORT` (so the bind
/// fails with `EADDRINUSE` when anything else holds the address, and no
/// other process can join it later); every later one gets a duplicate of
/// that socket, and `new` is not called. A listener on port 0 is the first
/// of the port the kernel gives it.
#[cfg(any(target_os = "macos", giong_libkqueue))]
fn bind_shared(
    proto: Proto,
    addr: SocketAddr,
    new: impl FnOnce() -> io::Result<Socket>,
) -> io::Result<(Socket, Option<Held>)> {
    // Held across the bind, as in `bind`.
    let mut ports = ports();
    let mut map = shared_sockets();
    let mut addr = addr;
    let known = match addr.port() {
        0 => None,
        _ => map.get(&(proto, addr)),
    };
    let (sock, shared) = match known {
        Some(shared) => (shared.try_clone()?, shared.clone()),
        None => {
            let sock = new()?;
            // As in `bind`: listeners only.
            if proto == Proto::Tcp {
                sock.set_reuse_address(true)?;
            }
            sock.bind(&addr.into())?;
            check_bound(&sock, addr)?;
            if addr.port() == 0 {
                let port = sock.local_addr()?.as_socket().map_or(0, |a| a.port());
                addr.set_port(port);
            }
            let shared = Arc::new(sock.try_clone()?);
            map.insert((proto, addr), shared.clone());
            (sock, shared)
        }
    };
    ports.entry((proto, addr.port())).or_default().live += 1;
    Ok((
        sock,
        Some(Held {
            proto,
            port: addr.port(),
            shared: Some((addr, shared)),
        }),
    ))
}

/// Lets the other workers' sockets bind the port `sock` binds.
#[cfg(unix)]
fn set_reuse_port(sock: &Socket) -> io::Result<()> {
    sock.set_reuse_port(true)
}

/// Windows has no `SO_REUSEPORT`: a port has one socket there, which the
/// workers' listeners and UDP sockets share as handles to it (see
/// `net::spread` and `net::udp::spread`); only the first of them binds.
#[cfg(windows)]
fn set_reuse_port(_sock: &Socket) -> io::Result<()> {
    Ok(())
}

/// Binds the first socket of a port in this process: without
/// `SO_REUSEPORT`, so that the bind fails with `EADDRINUSE` when a socket
/// that is not one of this process's released ones holds `addr`, then sets
/// it for the workers that join. `released` says whether there may be such
/// sockets.
///
/// Linux lets later sockets join a socket that took `SO_REUSEPORT` after its
/// bind. gVisor's network stack (GKE Sandbox, and sandboxes that run code
/// for AI agents) does not: it refuses them with `EADDRINUSE`, so a server
/// whose workers share a port would fail to start all but one. There the socket
/// whose bind made the check is closed, and a second one bound with
/// `SO_REUSEPORT` from the start (a TCP socket closed before it listens
/// gives its port up at once). Another process that binds the port in
/// between, with `SO_REUSEPORT` and as the same user, would then share it, a
/// window of a few microseconds. Where sockets of the port that this process
/// released may still be open, there is no check at all (see below).
fn bind_first(
    proto: Proto,
    new: impl Fn() -> io::Result<Socket>,
    addr: SocketAddr,
    released: bool,
    joins: Joins,
) -> io::Result<Socket> {
    let late = match joins {
        Joins::Find => late_reuse_port_joins(proto),
        #[cfg(all(test, unix))]
        Joins::Never => false,
    };
    // Two UDP sockets that both set `SO_REUSEADDR` never conflict, so this
    // lets the bind pass the sockets `Held::release` marked. It would also
    // pass another process's socket that sets it, so it is set only while
    // this process may have released sockets on the port, and only for the
    // bind.
    let marked = proto == Proto::Udp && released && cfg!(unix);
    if late || !released {
        let sock = new()?;
        if marked {
            sock.set_reuse_address(true)?;
        }
        sock.bind(&addr.into())?;
        check_bound(&sock, addr)?;
        if late {
            // The kernel looks at both options as they are when a later
            // socket binds, so the next worker joins this one.
            set_reuse_port(&sock)?;
            if marked {
                sock.set_reuse_address(false)?;
            }
            return Ok(sock);
        }
        // Closed again: its bind was the check.
    }
    // Where sockets this process released may still be open, they hold the
    // port with `SO_REUSEPORT`, and only a socket that sets it too can pass
    // them: there is no check then, a socket of another process that bound
    // the port since this one's were released is joined, not refused.
    let sock = new()?;
    set_reuse_port(&sock)?;
    sock.bind(&addr.into())?;
    check_bound(&sock, addr)?;
    Ok(sock)
}

/// A socket of `proto` for `addr`'s address family, with no options set.
#[cfg(unix)]
fn new_socket(proto: Proto, addr: SocketAddr) -> io::Result<Socket> {
    let ty = match proto {
        Proto::Tcp => Type::STREAM,
        Proto::Udp => Type::DGRAM,
    };
    Socket::new(Domain::for_address(addr), ty, None)
}

/// Whether a socket that set `SO_REUSEPORT` after it was bound (and, for
/// TCP, then listens) lets later sockets with `SO_REUSEPORT` join its port.
/// Linux does; gVisor does not, and refuses them with `EADDRINUSE` (a TCP
/// socket that does not listen yet it lets them join). Found out once per
/// process and transport, at the first bind of a shared port, in a few
/// system calls on a loopback port the kernel picks; where that cannot be
/// tried, Linux's answer. Not on the port being bound: a socket in its group,
/// even for a moment, would take a share of what is sent to it.
#[cfg(unix)]
fn late_reuse_port_joins(proto: Proto) -> bool {
    static TCP: OnceLock<bool> = OnceLock::new();
    static UDP: OnceLock<bool> = OnceLock::new();
    let once = match proto {
        Proto::Tcp => &TCP,
        Proto::Udp => &UDP,
    };
    *once.get_or_init(|| try_late_reuse_port(proto).unwrap_or(true))
}

/// Windows: only the first socket of a port binds (see [`set_reuse_port`]).
#[cfg(windows)]
fn late_reuse_port_joins(_proto: Proto) -> bool {
    true
}

#[cfg(unix)]
fn try_late_reuse_port(proto: Proto) -> io::Result<bool> {
    let any = SocketAddr::from((Ipv4Addr::LOCALHOST, 0));
    let first = new_socket(proto, any)?;
    first.set_reuse_address(proto == Proto::Tcp)?;
    first.bind(&any.into())?;
    first.set_reuse_port(true)?;
    if proto == Proto::Tcp {
        first.listen(1)?;
    }
    let Some(bound) = first.local_addr()?.as_socket() else {
        return Ok(true);
    };
    let second = new_socket(proto, bound)?;
    second.set_reuse_address(proto == Proto::Tcp)?;
    second.set_reuse_port(true)?;
    match second.bind(&bound.into()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => Ok(false),
        Err(e) => Err(e),
    }
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

    /// As [`udp`], the way it goes where a socket that takes `SO_REUSEPORT`
    /// after its bind lets no one join it (gVisor).
    #[cfg(unix)]
    fn udp_early(addr: SocketAddr) -> io::Result<(Socket, Option<Held>)> {
        bind_with(
            Proto::Udp,
            addr,
            true,
            || Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP)),
            Joins::Never,
        )
    }

    #[cfg(unix)]
    fn tcp_early(addr: SocketAddr) -> io::Result<(Socket, Option<Held>)> {
        bind_with(
            Proto::Tcp,
            addr,
            true,
            || Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP)),
            Joins::Never,
        )
    }

    fn known(addr: SocketAddr) -> Port {
        ports()
            .get(&(Proto::Udp, addr.port()))
            .copied()
            .unwrap_or_default()
    }

    #[test]
    #[cfg(all(target_os = "linux", not(giong_libkqueue)))]
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
    #[cfg(all(target_os = "linux", not(giong_libkqueue)))]
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
        let (first, second) = (Some(first), Some(second));
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
        let open = (first, second);
        let (third, third_hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 1);
        assert!(!third.reuse_address().unwrap());
        assert!(third.reuse_port().unwrap());
        let (fourth, fourth_hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 2);
        fourth_hold.unwrap().release(&fourth);
        third_hold.unwrap().release(&third);
        drop(open);
    }

    /// The macOS way for UDP: one socket without `SO_REUSEPORT`, duplicated
    /// for each worker, so that no other process (of any user) can bind the
    /// port and take its datagrams.
    #[test]
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    fn udp_sockets_of_one_address_share_one_socket_that_no_one_else_can_join() {
        let addr = free_port();
        // Another process's socket, as far as this module knows.
        let other = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        other.set_reuse_port(true).unwrap();
        other.bind(&addr.into()).unwrap();
        assert_eq!(udp(addr).unwrap_err().kind(), io::ErrorKind::AddrInUse);
        assert_eq!(known(addr).live, 0);
        drop(other);

        let (first, hold) = udp(addr).unwrap();
        assert!(!first.reuse_port().unwrap());
        assert!(!first.reuse_address().unwrap());
        // Nothing else can bind the address, with or without the options.
        let other = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        other.set_reuse_address(true).unwrap();
        other.set_reuse_port(true).unwrap();
        assert_eq!(
            other.bind(&addr.into()).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );

        // A second socket is the same one: a datagram sent once is
        // received by whichever reads first, here the second.
        let (second, second_hold) = udp(addr).unwrap();
        assert_eq!(known(addr).live, 2);
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        sender.send_to(b"x", addr).unwrap();
        let mut buf = [std::mem::MaybeUninit::new(0u8); 8];
        assert_eq!(second.recv(&mut buf).unwrap(), 1);

        // Dropping one leaves the other receiving.
        hold.unwrap().release(&first);
        drop(first);
        assert!(shared_sockets().contains_key(&(Proto::Udp, addr)));
        sender.send_to(b"y", addr).unwrap();
        assert_eq!(second.recv(&mut buf).unwrap(), 1);

        // The last one gives the address up: it can be bound again at once.
        second_hold.unwrap().release(&second);
        drop(second);
        assert!(!shared_sockets().contains_key(&(Proto::Udp, addr)));
        assert_eq!(known(addr).live, 0);
        let (third, third_hold) = udp(addr).unwrap();
        third_hold.unwrap().release(&third);
    }

    /// The macOS way: one listening socket, duplicated for each worker.
    #[test]
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    fn tcp_listeners_of_one_address_share_one_socket_until_the_last_is_dropped() {
        let tcp = |addr: SocketAddr| {
            bind(Proto::Tcp, addr, true, || {
                Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))
            })
        };
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let (first, hold) = tcp(addr).unwrap();
        first.listen(16).unwrap();
        assert!(!first.reuse_port().unwrap());
        // Nothing else can bind the address, with or without the option.
        let other = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        other.set_reuse_address(true).unwrap();
        other.set_reuse_port(true).unwrap();
        assert_eq!(
            other.bind(&addr.into()).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );

        // A second listener is the same socket: a connection made once is
        // accepted by whichever asks first, here the second.
        let (second, second_hold) = tcp(addr).unwrap();
        assert!(shared_sockets().contains_key(&(Proto::Tcp, addr)));
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (_conn, from) = second.accept().unwrap();
        assert_eq!(from.as_socket(), client.local_addr().ok());

        // Dropping one leaves the other listening.
        hold.unwrap().release(&first);
        drop(first);
        assert!(shared_sockets().contains_key(&(Proto::Tcp, addr)));
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (_conn, from) = second.accept().unwrap();
        assert_eq!(from.as_socket(), client.local_addr().ok());

        // The last one gives the address up: it can be bound again at once.
        second_hold.unwrap().release(&second);
        drop(second);
        assert!(!shared_sockets().contains_key(&(Proto::Tcp, addr)));
        let (third, third_hold) = tcp(addr).unwrap();
        third_hold.unwrap().release(&third);
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "Miri cannot set SO_REUSEPORT")]
    fn linux_lets_workers_join_a_socket_that_took_reuse_port_after_its_bind() {
        // What the first bind of a port relies on here; gVisor answers no.
        if std::env::var_os("GIONG_TEST_GVISOR").is_none() {
            assert!(late_reuse_port_joins(Proto::Tcp));
            assert!(late_reuse_port_joins(Proto::Udp));
        }
    }

    #[test]
    #[cfg(unix)]
    #[cfg_attr(miri, ignore = "Miri cannot set SO_REUSEPORT")]
    fn where_reuse_port_counts_at_the_bind_the_first_socket_checks_with_another() {
        // UDP: refused while another process's socket holds the port, then
        // the first socket and a worker that joins it.
        let addr = free_port();
        let other = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        other.set_reuse_port(true).unwrap();
        other.bind(&addr.into()).unwrap();
        let err = udp_early(addr).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert_eq!(known(addr).live, 0);
        drop(other);
        let (first, hold) = udp_early(addr).unwrap();
        assert!(!first.reuse_address().unwrap());
        assert!(first.reuse_port().unwrap());
        let (second, second_hold) = udp_early(addr).unwrap();
        assert_eq!(known(addr).live, 2);
        second_hold.unwrap().release(&second);
        hold.unwrap().release(&first);
        // Released and bound again, as a runtime started a second time does.
        let (third, third_hold) = udp_early(addr).unwrap();
        third_hold.unwrap().release(&third);

        // TCP: the same, with listeners.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let err = tcp_early(addr).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        drop(listener);
        let (first, hold) = tcp_early(addr).unwrap();
        first.listen(16).unwrap();
        assert!(first.reuse_port().unwrap());
        let (second, second_hold) = tcp_early(addr).unwrap();
        second.listen(16).unwrap();
        drop(std::net::TcpStream::connect(addr).unwrap());
        second_hold.unwrap().release(&second);
        hold.unwrap().release(&first);
    }
}
