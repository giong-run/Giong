//! Listening sockets handed down by `giong dev`, so that a service restarted
//! after a rebuild keeps its port and never refuses a connection.
//!
//! `giong dev` binds the service's listening sockets itself, once, and starts
//! each build of the service with them open and their numbers in
//! `GIONG_DEV_LISTEN` (`3=127.0.0.1:8080,4=[::1]:9000`). When the service calls
//! [`TcpListener::bind`](super::TcpListener::bind) for one of those addresses,
//! it gets a duplicate of the inherited socket instead of a new one. Every
//! worker that binds the address gets its own duplicate of the one socket, so
//! they share one queue of connections, as on macOS. A connection that arrives
//! while the old build stops and the new one starts waits in that queue: the
//! queue belongs to `giong dev`, which keeps the socket open across restarts.
//!
//! After each such bind, a byte (`\n`) is written to the socket numbered in
//! `GIONG_DEV_READY`, if set: `giong dev` reads it as "the new build is
//! accepting" and only then retires the old one, in two steps. First it
//! writes `s` to the old build's socket: its inherited listeners stop taking
//! connections from the shared queue (on io_uring, the multishot accept is
//! cancelled and what it already took is still handed to `accept`), and each
//! answers `d` once it holds nothing more. Then the old build gets `SIGTERM`.
//! A connection is thus never taken by a build that is about to close it.
//!
//! The variables apply only to the process `giong dev` started:
//! `GIONG_DEV_PARENT` names `giong dev`'s pid, and a process whose parent is
//! another (a program the service runs, which inherits its environment but
//! not its sockets) ignores them.
//!
//! On Linux, a service started this way gets `SIGTERM` when `giong dev` dies
//! (`PR_SET_PDEATHSIG`), so a killed `giong dev` leaves nothing behind
//! holding the port.
//!
//! Without `GIONG_DEV_LISTEN` nothing here runs past one environment lookup,
//! once per process. None of this is in a build without debug assertions
//! unless the `dev` feature is on (`giong dev --release` turns it on): such a
//! build has `inherit_off.rs` instead, which refuses to run under giong dev. The variable is for `giong dev` only; in production a
//! service binds its own sockets (or takes them from systemd).

use std::io;
use std::net::SocketAddr;
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll, Waker};

use socket2::{SockRef, Socket, Type};

use crate::driver::{Driver, DriverKind, MultiIo, MultiPoll};
use crate::error::{raise, Code};

/// `giong dev` asked this build to stop accepting.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// The tasks waiting in an inherited listener's `accept`, woken when
/// `giong dev` asks to stop so that they stop their accepts.
static WAITING: Mutex<Vec<Waker>> = Mutex::new(Vec::new());

/// The inherited sockets, parsed once: `Err` when the variable is malformed
/// (every bind then reports it).
static INHERITED: OnceLock<Result<Inherited, String>> = OnceLock::new();

#[derive(Debug, Default)]
struct Inherited {
    listeners: Vec<(RawFd, SocketAddr)>,
    ready: Option<RawFd>,
}

/// A duplicate of the inherited listening socket bound to `addr`, if
/// `giong dev` handed one down. Port 0 never matches.
pub(crate) fn take(addr: SocketAddr) -> io::Result<Option<Socket>> {
    if addr.port() == 0 {
        return Ok(None);
    }
    let inherited = INHERITED.get_or_init(|| {
        if !started_by_giong_dev() {
            return Ok(Inherited::default());
        }
        let listen = std::env::var("GIONG_DEV_LISTEN").ok();
        let ready = std::env::var("GIONG_DEV_READY").ok();
        parse(listen.as_deref(), ready.as_deref()).inspect(|i| {
            // The service's own child processes must not keep the sockets
            // (they would hold the port after giong dev stops).
            for &(fd, _) in &i.listeners {
                set_cloexec(fd);
            }
            if let Some(fd) = i.ready {
                set_cloexec(fd);
                listen_for_stop(fd);
            }
            if !i.listeners.is_empty() {
                stop_with_parent();
            }
        })
    });
    let inherited = match inherited {
        Ok(i) => i,
        Err(why) => {
            return Err(raise(
                Code::InvalidConfig,
                format!("GIONG_DEV_LISTEN or GIONG_DEV_READY is not valid: {why}"),
                "these variables are set by giong dev for the service it starts; unset them \
                 when running the service yourself.",
            ))
        }
    };
    let Some(&(fd, _)) = inherited.listeners.iter().find(|(_, a)| *a == addr) else {
        return Ok(None);
    };
    // SAFETY: the descriptor was handed to this process by giong dev and is
    // never closed by it: only duplicates are taken and closed. A descriptor
    // that is not a socket fails the calls below with ENOTSOCK.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let sock = SockRef::from(&borrowed);
    let not_it = |what: String| {
        raise(
            Code::InvalidConfig,
            format!("GIONG_DEV_LISTEN names descriptor {fd} for {addr}, but {what}"),
            "restart giong dev; when running the service yourself, unset GIONG_DEV_LISTEN.",
        )
    };
    let local = sock
        .local_addr()
        .map_err(|e| not_it(format!("it is not a socket ({e})")))?
        .as_socket();
    if local != Some(addr) {
        return Err(not_it(format!("it is bound to {local:?}")));
    }
    if sock.r#type().map_err(|e| not_it(e.to_string()))? != Type::STREAM {
        return Err(not_it("it is not a TCP socket".into()));
    }
    #[cfg(target_os = "linux")]
    if !sock.is_listener().map_err(|e| not_it(e.to_string()))? {
        return Err(not_it("it is not listening".into()));
    }
    // A duplicate that closes on exec (F_DUPFD_CLOEXEC).
    let dup = sock.try_clone()?;
    if let Some(ready) = inherited.ready {
        signal_ready(ready);
    }
    Ok(Some(dup))
}

/// Whether `giong dev` started this process: `GIONG_DEV_PARENT` is the pid
/// of the parent.
fn started_by_giong_dev() -> bool {
    let Some(parent) = std::env::var("GIONG_DEV_PARENT")
        .ok()
        .and_then(|p| p.trim().parse::<libc::pid_t>().ok())
    else {
        return false;
    };
    // SAFETY: getppid(2) cannot fail.
    parent == unsafe { libc::getppid() }
}

/// [`ListenSocket::poll_accept`](super::ListenSocket::poll_accept) for an
/// inherited socket: as usual until `giong dev` asks to stop; then what the
/// multishot accept already took is still returned, the operation is
/// cancelled and never armed again, and `d` tells `giong dev` this listener
/// holds nothing more.
pub(crate) fn poll_accept(
    sock: &Socket,
    key: usize,
    driver: &mut Driver,
    cx: &mut Context<'_>,
) -> Poll<io::Result<Socket>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let stopping = STOPPING.load(Ordering::Acquire);
    // The readiness drivers take a connection only when asked: nothing is
    // held, so stopping is immediate.
    if stopping && driver.kind() != DriverKind::IoUring {
        stopped();
        return Poll::Pending;
    }
    loop {
        match driver.poll_multi(key, cx.waker()) {
            MultiPoll::Item(res) if res >= 0 => {
                // SAFETY: a fresh descriptor returned by accept.
                return Poll::Ready(Ok(unsafe { Socket::from_raw_fd(res) }));
            }
            MultiPoll::Item(res) => return Poll::Ready(Err(io::Error::from_raw_os_error(-res))),
            MultiPoll::Data(_) => unreachable!("accept never receives data"),
            MultiPoll::NeedArm if stopping => {
                stopped();
                return Poll::Pending;
            }
            MultiPoll::NeedArm => {
                driver.arm_multi(
                    key,
                    MultiIo::Accept {
                        fd: sock.as_raw_fd(),
                    },
                );
            }
            MultiPoll::Pending => {
                if stopping {
                    // Its last completion wakes this task, which then finds
                    // it unarmed.
                    driver.restart_multi(key);
                } else {
                    wait_for_stop(cx.waker());
                }
                return Poll::Pending;
            }
        }
    }
}

fn wait_for_stop(waker: &Waker) {
    if let Ok(mut w) = WAITING.lock() {
        if !w.iter().any(|x| x.will_wake(waker)) {
            w.push(waker.clone());
        }
    }
    // The stop may have come between this poll's first look and the line
    // above, after the thread took the list: then nobody would wake this
    // task. (The thread sets the flag before it takes the list.)
    if STOPPING.load(Ordering::Acquire) {
        waker.wake_by_ref();
    }
}

/// One `d` to `giong dev`: this listener holds no connection.
fn stopped() {
    if let Some(Ok(i)) = INHERITED.get() {
        if let Some(fd) = i.ready {
            send_byte(fd, b'd');
        }
    }
}

/// A thread that waits for `giong dev`'s `s` on the ready socket, then wakes
/// every task waiting in an inherited listener's accept. It ends when
/// `giong dev` closes its end (or the descriptor is a pipe, which carries
/// nothing back).
fn listen_for_stop(fd: RawFd) {
    let spawned = std::thread::Builder::new()
        .name("giong-dev".into())
        .spawn(move || loop {
            let mut byte = 0u8;
            // SAFETY: `fd` stays open for the life of the process (only
            // duplicates are closed); the buffer is one valid byte.
            let n = unsafe { libc::recv(fd, (&mut byte as *mut u8).cast(), 1, 0) };
            if n == 1 && byte == b's' {
                STOPPING.store(true, Ordering::Release);
                let waiting = WAITING.lock().map(|mut w| std::mem::take(&mut *w));
                for w in waiting.into_iter().flatten() {
                    w.wake();
                }
            } else if n == 0
                || (n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted)
            {
                // Closed by giong dev, or not a socket (a pipe carries
                // nothing back).
                return;
            }
        });
    drop(spawned);
}

/// `3=127.0.0.1:8080,4=[::1]:9000` and `5`.
fn parse(listen: Option<&str>, ready: Option<&str>) -> Result<Inherited, String> {
    let mut out = Inherited::default();
    let fd = |s: &str| -> Result<RawFd, String> {
        match s.trim().parse::<RawFd>() {
            Ok(n) if n >= 0 => Ok(n),
            _ => Err(format!("{s:?} is not a descriptor number")),
        }
    };
    if let Some(listen) = listen.filter(|l| !l.trim().is_empty()) {
        for entry in listen.split(',') {
            let (n, addr) = entry
                .split_once('=')
                .ok_or_else(|| format!("{entry:?} is not <descriptor>=<address>"))?;
            let addr: SocketAddr = addr
                .trim()
                .parse()
                .map_err(|_| format!("{addr:?} is not an IP address and port"))?;
            out.listeners.push((fd(n)?, addr));
        }
    }
    if let Some(r) = ready.filter(|r| !r.trim().is_empty()) {
        out.ready = Some(fd(r)?);
    }
    Ok(out)
}

fn set_cloexec(fd: RawFd) {
    // SAFETY: fcntl on a descriptor number only reads and sets its flags; a
    // number that is not open fails with EBADF, which is ignored (the bind
    // then reports the descriptor).
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// One byte to giong dev's ready descriptor (a socket or a pipe). Never
/// blocks the worker: a full buffer (giong dev not reading) or a closed one
/// only loses the signal, and never raises `SIGPIPE`.
fn signal_ready(fd: RawFd) {
    send_byte(fd, b'\n');
}

fn send_byte(fd: RawFd, byte: u8) {
    // One sender at a time: on macOS, a non-blocking send(2) fails with
    // EAGAIN, and the byte is lost, while another thread is sending on the
    // same socket (the workers each report their binds at the same moment).
    static SENDING: Mutex<()> = Mutex::new(());
    let _one = SENDING.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: `fd` is a descriptor giong dev handed down and keeps open in
    // this process; the buffer is one valid byte. On a socket, send(2) with
    // MSG_DONTWAIT | MSG_NOSIGNAL neither blocks nor signals; on a pipe
    // (ENOTSOCK) the descriptor is made non-blocking and written.
    unsafe {
        let sent = loop {
            let sent = libc::send(
                fd,
                (&byte as *const u8).cast(),
                1,
                libc::MSG_DONTWAIT | NOSIGNAL,
            );
            if sent >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break sent;
            }
        };
        if sent < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSOCK) {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 && flags & libc::O_NONBLOCK == 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
const NOSIGNAL: libc::c_int = libc::MSG_NOSIGNAL;
// macOS has no MSG_NOSIGNAL; Rust programs ignore SIGPIPE from the start.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const NOSIGNAL: libc::c_int = 0;

/// On Linux, the service gets SIGTERM when giong dev dies, so a killed
/// giong dev leaves no service holding its port.
#[cfg(target_os = "linux")]
fn stop_with_parent() {
    // SAFETY: prctl(PR_SET_PDEATHSIG) only sets a flag of this process.
    unsafe {
        libc::prctl(
            libc::PR_SET_PDEATHSIG,
            libc::SIGTERM as libc::c_ulong,
            0,
            0,
            0,
        );
    }
    // (giong dev starts services from its main thread, which lives as long
    // as it does: the flag follows the thread that started the process.)
}

#[cfg(not(target_os = "linux"))]
fn stop_with_parent() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variables() {
        let i = parse(Some("3=127.0.0.1:8080, 4=[::1]:9000"), Some("5")).unwrap();
        assert_eq!(
            i.listeners,
            [
                (3, "127.0.0.1:8080".parse().unwrap()),
                (4, "[::1]:9000".parse().unwrap())
            ]
        );
        assert_eq!(i.ready, Some(5));
        let none = parse(None, None).unwrap();
        assert!(none.listeners.is_empty() && none.ready.is_none());
        assert!(parse(Some(""), Some(" ")).unwrap().listeners.is_empty());
        for bad in [
            "3",
            "x=127.0.0.1:1",
            "3=localhost:80",
            "-1=127.0.0.1:1",
            "3=127.0.0.1",
        ] {
            assert!(parse(Some(bad), None).is_err(), "{bad}");
        }
        assert!(parse(None, Some("r")).is_err());
    }
}
