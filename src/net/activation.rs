//! Socket activation: listening sockets handed over by systemd (or another
//! service manager speaking its protocol) when the service starts.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};

use super::{TcpListener, UdpSocket, UnixDatagram, UnixListener};
use crate::error::{raise, Code};

/// The first descriptor a service manager passes (`SD_LISTEN_FDS_START`).
const FIRST_FD: RawFd = 3;

static TAKEN: AtomicBool = AtomicBool::new(false);

/// A socket this process inherited from its service manager, from
/// [`activated_sockets`].
///
/// It owns the inherited descriptor and is `Send` and `Sync`: share the list
/// with the workers (in an `Arc`), and let each worker make its own
/// listener with [`tcp_listener`](Self::tcp_listener) (or the method for the
/// socket's kind). The listeners share the one socket and its queue of
/// connections, so the workers take turns accepting from it.
#[derive(Debug)]
pub struct ActivatedSocket {
    fd: OwnedFd,
    name: Option<String>,
}

/// Takes the sockets the service manager passed to this process: systemd's
/// socket activation (`LISTEN_FDS`, `LISTEN_PID`, `LISTEN_FDNAMES`; see
/// `sd_listen_fds(3)`), also spoken by other managers, such as
/// `systemd-socket-activate` and `s6`.
///
/// Returns an empty list when the process was started without any (no
/// `LISTEN_FDS`, or `LISTEN_PID` names another process): a service can
/// then bind its own sockets, which makes the same binary run both under
/// systemd and from a shell.
///
/// Call it once, early in `main`, before anything else opens descriptors
/// that might be confused with the inherited ones. A second call fails with
/// [`Code::ActivatedSocket`], because the descriptors would be owned twice.
/// The environment variables are left as they are (changing the environment
/// of a running program with threads is unsound), and the descriptors are
/// marked close-on-exec so that children do not inherit them.
///
/// ```no_run
/// use std::sync::Arc;
///
/// let sockets = Arc::new(giong::net::activated_sockets()?);
/// giong::Builder::new().build()?.run(move |_worker| {
///     let sockets = sockets.clone();
///     async move {
///         let listener = match sockets.first() {
///             Some(socket) => socket.tcp_listener().expect("a TCP listener"),
///             None => giong::net::TcpListener::bind("0.0.0.0:8080").expect("bind"),
///         };
///         while let Ok(stream) = listener.accept().await {
///             drop(stream);
///         }
///     }
/// })?;
/// # Ok::<_, std::io::Error>(())
/// ```
pub fn activated_sockets() -> io::Result<Vec<ActivatedSocket>> {
    if TAKEN.swap(true, Ordering::SeqCst) {
        return Err(raise(
            Code::ActivatedSocket,
            "activated_sockets was called a second time; the inherited sockets were already \
             taken",
            "call activated_sockets once, early in main, and share the list it returns (in an \
             Arc) with the workers.",
        ));
    }
    let pid_matches = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .is_some_and(|p| p == std::process::id());
    let count = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|n| n.trim().parse::<i32>().ok())
        .unwrap_or(0);
    if !pid_matches || count <= 0 {
        return Ok(Vec::new());
    }
    let names: Vec<String> = std::env::var("LISTEN_FDNAMES")
        .map(|n| n.split(':').map(str::to_owned).collect())
        .unwrap_or_default();
    let mut sockets = Vec::with_capacity(count as usize);
    for i in 0..count {
        let fd = FIRST_FD + i;
        // SAFETY: fcntl on a descriptor number; a closed one fails.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the service manager passed this descriptor to this
        // process, which takes ownership of it here, once (`TAKEN`).
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let name = names
            .get(i as usize)
            .filter(|n| !n.is_empty() && n.as_str() != "unknown")
            .cloned();
        sockets.push(ActivatedSocket { fd, name });
    }
    Ok(sockets)
}

/// What a socket is: its address family, type, and whether it listens.
struct Kind {
    domain: i32,
    ty: i32,
    listening: bool,
}

fn int_option(fd: RawFd, option: i32) -> io::Result<i32> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: a valid pointer to an int and its length.
    let res = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            option,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if res != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

impl Kind {
    fn of(fd: RawFd) -> io::Result<Self> {
        Ok(Self {
            domain: int_option(fd, libc::SO_DOMAIN)?,
            ty: int_option(fd, libc::SO_TYPE)?,
            listening: int_option(fd, libc::SO_ACCEPTCONN)? != 0,
        })
    }

    fn describe(&self) -> String {
        let family = match self.domain {
            libc::AF_INET => "IPv4",
            libc::AF_INET6 => "IPv6",
            libc::AF_UNIX => "Unix",
            _ => "other-family",
        };
        let ty = match self.ty {
            libc::SOCK_STREAM if self.listening => "listening stream",
            libc::SOCK_STREAM => "stream (not listening)",
            libc::SOCK_DGRAM => "datagram",
            libc::SOCK_SEQPACKET => "seqpacket",
            _ => "other-type",
        };
        format!("{family} {ty} socket")
    }
}

impl ActivatedSocket {
    /// The name the service manager gave the socket (`FileDescriptorName=`
    /// in a systemd `.socket` unit), if any. Pick sockets by name rather
    /// than by position when there are several.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Checks the socket is of the kind `wanted` describes, and returns a
    /// copy of its descriptor for this worker.
    fn copy_if(
        &self,
        method: &str,
        wanted: &str,
        ok: impl Fn(&Kind) -> bool,
    ) -> io::Result<OwnedFd> {
        let kind = Kind::of(self.fd.as_raw_fd())?;
        if !ok(&kind) {
            let name = self
                .name
                .as_deref()
                .map(|n| format!(" \"{n}\""))
                .unwrap_or_default();
            return Err(raise(
                Code::ActivatedSocket,
                format!(
                    "{method} was called on the inherited socket{name}, which is an {} rather \
                     than {wanted}",
                    kind.describe()
                ),
                "call the method for the socket's kind (tcp_listener, unix_listener, udp_socket, \
                 unix_datagram), and check the socket unit: ListenStream= with a port is a TCP \
                 listener, with a path a Unix listener; ListenDatagram= with a port is UDP, with \
                 a path a Unix datagram socket.",
            ));
        }
        self.fd.try_clone()
    }

    /// A [`TcpListener`] on this worker for the socket, which must be a
    /// listening TCP socket (`ListenStream=` with an address or port). Call
    /// it on each worker that should accept connections.
    pub fn tcp_listener(&self) -> io::Result<TcpListener> {
        let fd = self.copy_if("tcp_listener", "a listening TCP socket", |k| {
            matches!(k.domain, libc::AF_INET | libc::AF_INET6)
                && k.ty == libc::SOCK_STREAM
                && k.listening
        })?;
        Ok(TcpListener::from_std(std::net::TcpListener::from(fd)))
    }

    /// A [`UnixListener`] on this worker for the socket, which must be a
    /// listening Unix stream socket (`ListenStream=` with a path).
    pub fn unix_listener(&self) -> io::Result<UnixListener> {
        let fd = self.copy_if("unix_listener", "a listening Unix stream socket", |k| {
            k.domain == libc::AF_UNIX && k.ty == libc::SOCK_STREAM && k.listening
        })?;
        Ok(UnixListener::from_std(
            std::os::unix::net::UnixListener::from(fd),
        ))
    }

    /// A [`UdpSocket`] on this worker for the socket, which must be a UDP
    /// socket (`ListenDatagram=` with a port).
    pub fn udp_socket(&self) -> io::Result<UdpSocket> {
        let fd = self.copy_if("udp_socket", "a UDP socket", |k| {
            matches!(k.domain, libc::AF_INET | libc::AF_INET6) && k.ty == libc::SOCK_DGRAM
        })?;
        Ok(UdpSocket::from_std(std::net::UdpSocket::from(fd)))
    }

    /// A [`UnixDatagram`] on this worker for the socket, which must be a
    /// Unix datagram socket (`ListenDatagram=` with a path).
    pub fn unix_datagram(&self) -> io::Result<UnixDatagram> {
        let fd = self.copy_if("unix_datagram", "a Unix datagram socket", |k| {
            k.domain == libc::AF_UNIX && k.ty == libc::SOCK_DGRAM
        })?;
        Ok(UnixDatagram::from_std(
            std::os::unix::net::UnixDatagram::from(fd),
        ))
    }

    /// The inherited descriptor itself, for a kind of socket not covered
    /// above.
    pub fn into_owned_fd(self) -> OwnedFd {
        self.fd
    }
}

impl AsFd for ActivatedSocket {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl AsRawFd for ActivatedSocket {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
