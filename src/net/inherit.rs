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
//! After each such bind, a byte is written to the pipe numbered in
//! `GIONG_DEV_READY`, if set: `giong dev` reads it as "the new build is
//! accepting" and only then tells the old one to stop.
//!
//! Without `GIONG_DEV_LISTEN` nothing here runs past one environment lookup,
//! once per process. The variable is for `giong dev` only; in production a
//! service binds its own sockets (or takes them from systemd).

use std::io;
use std::net::SocketAddr;
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::OnceLock;

use socket2::{SockRef, Socket, Type};

use crate::error::{raise, Code};

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

/// One byte to giong dev's pipe. Never blocks the worker: a full pipe (giong
/// dev not reading) or a closed one only loses the signal.
fn signal_ready(fd: RawFd) {
    // SAFETY: `fd` is the write end of a pipe giong dev handed down and keeps
    // open in this process; the buffer is one valid byte. MSG_DONTWAIT does
    // not apply to pipes, so the descriptor is made non-blocking first.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 && flags & libc::O_NONBLOCK == 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        libc::write(fd, b"\n".as_ptr().cast(), 1);
    }
}

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
