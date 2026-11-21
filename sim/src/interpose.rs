//! The libc functions the simulator takes over. A binary that links this
//! crate resolves these symbols here instead of in libc. For a thread
//! outside the simulation every function forwards to the real system call;
//! for a simulated thread it acts on the simulated world.
//!
//! Variadic functions (`syscall`, `fcntl`, `ioctl`, `openat`) are declared
//! with fixed integer arguments: on x86-64 those arrive in the same
//! registers either way.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{c_char, c_int, c_long, c_uint, c_void, CStr};
use std::mem::size_of;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

use libc::{size_t, sockaddr, socklen_t, ssize_t};

use crate::files;
use crate::net::{self, Addr, Kind, ObjId, State};
use crate::raw::{self, fail, libc_ret, syscall3, syscall6};
use crate::world::{self, Guard, Tid, Why, World, REALTIME_AT_T0, T0};

// ------------------------------------------------------------------ helpers

/// Enters the simulation for this call: `None` for a thread outside it.
fn sim() -> Option<(Guard, Tid)> {
    let (g, me) = world::enter()?;
    let g = world::step(g, me);
    if g.as_ref().is_none_or(|w| w.failure.is_some()) {
        abort_failed();
    }
    Some((g, me))
}

/// Enters the simulation for a call on descriptor `fd`. Calls on real
/// descriptors (the test's own files, a panic message being printed) take
/// no virtual time and draw nothing from the seed, so incidental I/O that
/// happens only in some runs (the first panic of a process loads debug
/// information) cannot shift the run.
fn sim_fd(fd: c_int) -> Option<(Guard, Tid)> {
    let (g, me) = world::enter()?;
    if g.as_ref().is_some_and(|w| w.net.obj(fd).is_some()) {
        let g = world::step(g, me);
        if g.as_ref().is_none_or(|w| w.failure.is_some()) {
            abort_failed();
        }
        return Some((g, me));
    }
    Some((g, me))
}

fn abort_failed() -> ! {
    raw::eprint("giong-sim: aborting the failed run (report above)\n");
    std::process::abort();
}

fn w(g: &mut Guard) -> &mut World {
    g.as_mut().unwrap()
}

/// After a block: the run may have failed meanwhile.
fn check(g: &Guard) {
    if g.as_ref().is_none_or(|w| w.failure.is_some()) {
        abort_failed();
    }
}

static PLACEHOLDER: AtomicI32 = AtomicI32::new(-1);

/// A real descriptor number for a simulated object: a duplicate of
/// `/dev/null`, so the number is unique in the process.
fn new_fd(w: &mut World, obj: ObjId) -> i32 {
    let mut ph = PLACEHOLDER.load(Ordering::Relaxed);
    if ph < 0 {
        let path = c"/dev/null";
        // SAFETY: a valid C string.
        ph = unsafe {
            syscall6(
                libc::SYS_openat,
                libc::AT_FDCWD as i64,
                path.as_ptr() as i64,
                (libc::O_RDONLY | libc::O_CLOEXEC) as i64,
                0,
                0,
                0,
            )
        } as i32;
        assert!(ph >= 0, "giong-sim: cannot open /dev/null");
        PLACEHOLDER.store(ph, Ordering::Relaxed);
    }
    // SAFETY: duplicating a descriptor we own.
    let fd =
        unsafe { syscall3(libc::SYS_fcntl, ph as i64, libc::F_DUPFD_CLOEXEC as i64, 3) } as i32;
    assert!(fd >= 0, "giong-sim: out of descriptors");
    w.net.fds.insert(fd, obj);
    if let Some(o) = w.net.objs.get_mut(&obj) {
        o.refs += 1;
    }
    fd
}

unsafe fn read_addr(ptr: *const sockaddr, len: socklen_t) -> Result<Addr, i32> {
    if ptr.is_null() || (len as usize) < size_of::<libc::sa_family_t>() {
        return Err(libc::EINVAL);
    }
    match (*ptr).sa_family as i32 {
        libc::AF_INET if len as usize >= size_of::<libc::sockaddr_in>() => {
            let a = &*(ptr as *const libc::sockaddr_in);
            let ip = Ipv4Addr::from(u32::from_be(a.sin_addr.s_addr));
            Ok(Addr::Inet(SocketAddr::V4(SocketAddrV4::new(
                ip,
                u16::from_be(a.sin_port),
            ))))
        }
        libc::AF_INET6 if len as usize >= size_of::<libc::sockaddr_in6>() => {
            let a = &*(ptr as *const libc::sockaddr_in6);
            let ip = Ipv6Addr::from(a.sin6_addr.s6_addr);
            // An IPv4-mapped address is IPv4 for our purposes.
            if let Some(v4) = ip.to_ipv4_mapped() {
                return Ok(Addr::Inet(SocketAddr::V4(SocketAddrV4::new(
                    v4,
                    u16::from_be(a.sin6_port),
                ))));
            }
            Ok(Addr::Inet(SocketAddr::V6(SocketAddrV6::new(
                ip,
                u16::from_be(a.sin6_port),
                a.sin6_flowinfo,
                a.sin6_scope_id,
            ))))
        }
        libc::AF_UNIX => {
            let a = &*(ptr as *const libc::sockaddr_un);
            let off = size_of::<libc::sa_family_t>();
            let n = (len as usize).saturating_sub(off).min(a.sun_path.len());
            let mut path: Vec<u8> = a.sun_path[..n].iter().map(|c| *c as u8).collect();
            if path.first() != Some(&0) {
                if let Some(end) = path.iter().position(|b| *b == 0) {
                    path.truncate(end);
                }
            }
            Ok(Addr::Unix(path))
        }
        _ => Err(libc::EAFNOSUPPORT),
    }
}

unsafe fn write_addr(addr: &Addr, domain: i32, ptr: *mut sockaddr, lenp: *mut socklen_t) {
    if ptr.is_null() || lenp.is_null() {
        return;
    }
    let mut st: libc::sockaddr_storage = std::mem::zeroed();
    let len = match addr {
        Addr::Inet(SocketAddr::V4(a)) if domain == libc::AF_INET6 => {
            let s = &mut *(&mut st as *mut _ as *mut libc::sockaddr_in6);
            s.sin6_family = libc::AF_INET6 as u16;
            s.sin6_port = a.port().to_be();
            s.sin6_addr.s6_addr = a.ip().to_ipv6_mapped().octets();
            size_of::<libc::sockaddr_in6>()
        }
        Addr::Inet(SocketAddr::V4(a)) => {
            let s = &mut *(&mut st as *mut _ as *mut libc::sockaddr_in);
            s.sin_family = libc::AF_INET as u16;
            s.sin_port = a.port().to_be();
            s.sin_addr.s_addr = u32::from(*a.ip()).to_be();
            size_of::<libc::sockaddr_in>()
        }
        Addr::Inet(SocketAddr::V6(a)) => {
            let s = &mut *(&mut st as *mut _ as *mut libc::sockaddr_in6);
            s.sin6_family = libc::AF_INET6 as u16;
            s.sin6_port = a.port().to_be();
            s.sin6_addr.s6_addr = a.ip().octets();
            s.sin6_flowinfo = a.flowinfo();
            s.sin6_scope_id = a.scope_id();
            size_of::<libc::sockaddr_in6>()
        }
        Addr::Unix(p) => {
            let s = &mut *(&mut st as *mut _ as *mut libc::sockaddr_un);
            s.sun_family = libc::AF_UNIX as u16;
            let n = p.len().min(s.sun_path.len() - 1);
            for (i, b) in p[..n].iter().enumerate() {
                s.sun_path[i] = *b as c_char;
            }
            size_of::<libc::sa_family_t>() + n + usize::from(p.first() != Some(&0) && !p.is_empty())
        }
    };
    let cap = *lenp as usize;
    std::ptr::copy_nonoverlapping(&st as *const _ as *const u8, ptr as *mut u8, cap.min(len));
    *lenp = len as socklen_t;
}

fn unspecified(domain: i32) -> Addr {
    if domain == libc::AF_INET6 {
        Addr::Inet(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0))
    } else if domain == libc::AF_UNIX {
        Addr::Unix(Vec::new())
    } else {
        Addr::Inet(SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0))
    }
}

/// Repeats `op` until it does not return EAGAIN, blocking for `mask` in
/// between unless the object is non-blocking or `dontwait`.
fn blocking<T>(
    mut g: Guard,
    me: Tid,
    obj: ObjId,
    mask: u32,
    dontwait: bool,
    mut op: impl FnMut(&mut World) -> Result<T, i32>,
) -> (Guard, Result<T, i32>) {
    loop {
        let r = op(w(&mut g));
        match r {
            Err(libc::EAGAIN) => {
                let nonblock = w(&mut g).net.objs.get(&obj).is_none_or(|o| o.nonblock);
                if nonblock || dontwait {
                    return (g, r);
                }
                let (g2, res) = world::block(g, me, Why::Fd { obj, mask }, None);
                g = g2;
                check(&g);
                if res != 0 {
                    return (g, Err(libc::EINTR));
                }
            }
            r => return (g, r),
        }
    }
}

/// Injected spurious EAGAIN before a non-blocking call on `obj`. It models
/// data that arrives just after the check, so it comes with a new readiness
/// edge (with edge-triggered epoll, real Linux never reports EAGAIN while
/// data is queued without one following). Blocking calls get none: they
/// only fail that way on a socket timeout. No EINTR either: a call that
/// does not sleep cannot be interrupted, and signals are not simulated, so
/// EINTR is injected only where Gióng sleeps (`epoll_wait`, futex).
fn injected_error(w: &mut World, obj: ObjId, may_eagain: bool, flags: c_int) -> Option<i32> {
    let nonblock =
        flags & libc::MSG_DONTWAIT != 0 || w.net.objs.get(&obj).is_some_and(|o| o.nonblock);
    let p = w.faults.spurious_eagain;
    if nonblock && may_eagain && w.fault(p, "spurious-eagain") {
        w.net.bump(obj);
        return Some(libc::EAGAIN);
    }
    None
}

fn res_str(r: &Result<usize, i32>) -> String {
    match r {
        Ok(n) => n.to_string(),
        Err(e) => errname(*e),
    }
}

pub(crate) fn errname(e: i32) -> String {
    let name = match e {
        libc::EAGAIN => "EAGAIN",
        libc::EINTR => "EINTR",
        libc::ECONNRESET => "ECONNRESET",
        libc::ECONNREFUSED => "ECONNREFUSED",
        libc::EPIPE => "EPIPE",
        libc::ENOTCONN => "ENOTCONN",
        libc::EINPROGRESS => "EINPROGRESS",
        libc::ETIMEDOUT => "ETIMEDOUT",
        libc::EBADF => "EBADF",
        libc::EINVAL => "EINVAL",
        libc::EADDRINUSE => "EADDRINUSE",
        libc::EIO => "EIO",
        libc::ENOSPC => "ENOSPC",
        libc::ECONNABORTED => "ECONNABORTED",
        libc::EALREADY => "EALREADY",
        libc::EISCONN => "EISCONN",
        libc::ENOENT => "ENOENT",
        libc::ENOPROTOOPT => "ENOPROTOOPT",
        libc::ENOSYS => "ENOSYS",
        _ => return format!("E{e}"),
    };
    name.to_string()
}

fn ret(r: Result<usize, i32>) -> ssize_t {
    match r {
        Ok(n) => n as ssize_t,
        Err(e) => fail(e) as ssize_t,
    }
}

fn obj_of(w: &World, fd: c_int) -> Option<ObjId> {
    w.net.obj(fd)
}

// ------------------------------------------------------------------ sockets

#[no_mangle]
pub unsafe extern "C" fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int {
    let Some((mut g, me)) = sim() else {
        return libc_ret(syscall3(
            libc::SYS_socket,
            domain as i64,
            ty as i64,
            protocol as i64,
        )) as c_int;
    };
    let w = w(&mut g);
    let base = ty & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC);
    let stream = match base {
        libc::SOCK_STREAM => true,
        libc::SOCK_DGRAM => false,
        _ => {
            w.trace(me, format_args!("socket type {base} unsupported"));
            return fail(libc::EPROTONOSUPPORT) as c_int;
        }
    };
    if !matches!(domain, libc::AF_INET | libc::AF_INET6 | libc::AF_UNIX) {
        return fail(libc::EAFNOSUPPORT) as c_int;
    }
    let id = w.net.add(
        net::sock_kind(domain, stream),
        ty & libc::SOCK_NONBLOCK != 0,
    );
    let fd = new_fd(w, id);
    w.trace(
        me,
        format_args!(
            "socket {} {} -> o{id}",
            domain,
            if stream { "stream" } else { "dgram" }
        ),
    );
    fd
}

#[no_mangle]
pub unsafe extern "C" fn socketpair(
    domain: c_int,
    ty: c_int,
    protocol: c_int,
    sv: *mut c_int,
) -> c_int {
    let Some((mut g, me)) = sim() else {
        return libc_ret(syscall6(
            libc::SYS_socketpair,
            domain as i64,
            ty as i64,
            protocol as i64,
            sv as i64,
            0,
            0,
        )) as c_int;
    };
    let w = w(&mut g);
    if domain != libc::AF_UNIX
        || ty & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC) != libc::SOCK_STREAM
    {
        return fail(libc::EOPNOTSUPP) as c_int;
    }
    let (a, b) = w.socketpair(domain, ty & libc::SOCK_NONBLOCK != 0);
    *sv = new_fd(w, a);
    *sv.add(1) = new_fd(w, b);
    w.trace(me, format_args!("socketpair -> o{a} o{b}"));
    0
}

#[no_mangle]
pub unsafe extern "C" fn bind(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    let Some((mut g, me)) = sim_fd(fd) else {
        return libc_ret(syscall3(libc::SYS_bind, fd as i64, addr as i64, len as i64)) as c_int;
    };
    let w = w(&mut g);
    let Some(id) = obj_of(w, fd) else {
        return libc_ret(syscall3(libc::SYS_bind, fd as i64, addr as i64, len as i64)) as c_int;
    };
    let r = match read_addr(addr, len) {
        Ok(a) => w.net.bind(id, a.clone()).map(|_| 0),
        Err(e) => Err(e),
    };
    let local = w
        .net
        .sock(id)
        .and_then(|s| s.local.clone())
        .map(|a| a.to_string())
        .unwrap_or_default();
    w.trace(me, format_args!("bind o{id} {local} -> {}", res_str(&r)));
    ret(r) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn listen(fd: c_int, backlog: c_int) -> c_int {
    let Some((mut g, me)) = sim_fd(fd) else {
        return libc_ret(syscall3(libc::SYS_listen, fd as i64, backlog as i64, 0)) as c_int;
    };
    let w = w(&mut g);
    let Some(id) = obj_of(w, fd) else {
        return libc_ret(syscall3(libc::SYS_listen, fd as i64, backlog as i64, 0)) as c_int;
    };
    let r = w.listen(id, backlog).map(|_| 0);
    w.trace(me, format_args!("listen o{id} -> {}", res_str(&r)));
    ret(r) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn accept(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int {
    accept4(fd, addr, len, 0)
}

#[no_mangle]
pub unsafe extern "C" fn accept4(
    fd: c_int,
    addr: *mut sockaddr,
    len: *mut socklen_t,
    flags: c_int,
) -> c_int {
    let Some((mut g, me)) = sim_fd(fd) else {
        return libc_ret(syscall6(
            libc::SYS_accept4,
            fd as i64,
            addr as i64,
            len as i64,
            flags as i64,
            0,
            0,
        )) as c_int;
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        return libc_ret(syscall6(
            libc::SYS_accept4,
            fd as i64,
            addr as i64,
            len as i64,
            flags as i64,
            0,
            0,
        )) as c_int;
    };
    if let Some(e) = injected_error(w(&mut g), id, true, 0) {
        w(&mut g).trace(
            me,
            format_args!("accept o{id} -> {} (injected)", errname(e)),
        );
        return fail(e) as c_int;
    }
    let (mut g, r) = blocking(g, me, id, net::IN, false, |w| w.accept(id));
    let w = w(&mut g);
    match r {
        Ok(new) => {
            let p = w.faults.accept_abort;
            if w.fault(p, "accept-abort") {
                // The connection was reset before accept took it.
                if let Some(State::Connected { conn, side }) = w.net.sock(new).map(|s| &s.state) {
                    let (conn, side) = (*conn, *side);
                    w.reset_conn(conn, Some(side));
                }
                w.destroy(new);
                w.trace(me, format_args!("accept o{id} -> ECONNABORTED (injected)"));
                return fail(libc::ECONNABORTED) as c_int;
            }
            if let Some(o) = w.net.objs.get_mut(&new) {
                o.nonblock = flags & libc::SOCK_NONBLOCK != 0;
            }
            let nfd = new_fd(w, new);
            let domain = w.net.sock(new).map_or(libc::AF_INET, |s| s.domain);
            if let Ok(peer) = w.peer_addr(new) {
                write_addr(&peer, domain, addr, len);
            }
            w.trace(me, format_args!("accept o{id} -> o{new}"));
            nfd
        }
        Err(e) => {
            w.trace(me, format_args!("accept o{id} -> {}", errname(e)));
            fail(e) as c_int
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn connect(fd: c_int, addr: *const sockaddr, len: socklen_t) -> c_int {
    let Some((mut g, me)) = sim_fd(fd) else {
        return libc_ret(syscall3(
            libc::SYS_connect,
            fd as i64,
            addr as i64,
            len as i64,
        )) as c_int;
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        return libc_ret(syscall3(
            libc::SYS_connect,
            fd as i64,
            addr as i64,
            len as i64,
        )) as c_int;
    };
    let to = match read_addr(addr, len) {
        Ok(a) => a,
        Err(e) => return fail(e) as c_int,
    };
    let wr = w(&mut g);
    let stream = wr.net.sock(id).is_some_and(|s| s.stream);
    if !stream {
        let r = wr.connect_dgram(id, to.clone()).map(|_| 0);
        wr.trace(me, format_args!("connect o{id} {to} -> {}", res_str(&r)));
        return ret(r) as c_int;
    }
    let r = wr.connect_stream(id, to.clone());
    wr.trace(
        me,
        format_args!(
            "connect o{id} {to} -> {}",
            r.map_or_else(errname, |done| if done {
                "0".into()
            } else {
                "started".into()
            })
        ),
    );
    match r {
        Err(e) => return fail(e) as c_int,
        Ok(true) => return 0,
        Ok(false) => {}
    }
    let nonblock = wr.net.objs.get(&id).is_some_and(|o| o.nonblock);
    if nonblock {
        return fail(libc::EINPROGRESS) as c_int;
    }
    // Blocking connect: wait until it is no longer in progress.
    let (mut g, _) = blocking(
        g,
        me,
        id,
        net::OUT | net::ERR | net::HUP,
        false,
        |w| match w.net.sock(id).map(|s| &s.state) {
            Some(State::Connecting) => Err(libc::EAGAIN),
            _ => Ok(()),
        },
    );
    let w = w(&mut g);
    let s = w.net.sock_mut(id).unwrap();
    if s.so_error != 0 {
        let e = std::mem::take(&mut s.so_error);
        return fail(e) as c_int;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn getsockname(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int {
    let Some((mut g, _me)) = sim_fd(fd) else {
        return libc_ret(syscall3(
            libc::SYS_getsockname,
            fd as i64,
            addr as i64,
            len as i64,
        )) as c_int;
    };
    let w = w(&mut g);
    let Some(id) = obj_of(w, fd) else {
        return libc_ret(syscall3(
            libc::SYS_getsockname,
            fd as i64,
            addr as i64,
            len as i64,
        )) as c_int;
    };
    let Some(s) = w.net.sock(id) else {
        return fail(libc::ENOTSOCK) as c_int;
    };
    let a = s.local.clone().unwrap_or_else(|| unspecified(s.domain));
    write_addr(&a, s.domain, addr, len);
    0
}

#[no_mangle]
pub unsafe extern "C" fn getpeername(fd: c_int, addr: *mut sockaddr, len: *mut socklen_t) -> c_int {
    let Some((mut g, _me)) = sim_fd(fd) else {
        return libc_ret(syscall3(
            libc::SYS_getpeername,
            fd as i64,
            addr as i64,
            len as i64,
        )) as c_int;
    };
    let w = w(&mut g);
    let Some(id) = obj_of(w, fd) else {
        return libc_ret(syscall3(
            libc::SYS_getpeername,
            fd as i64,
            addr as i64,
            len as i64,
        )) as c_int;
    };
    let domain = w.net.sock(id).map_or(libc::AF_INET, |s| s.domain);
    match w.peer_addr(id) {
        Ok(a) => {
            write_addr(&a, domain, addr, len);
            0
        }
        Err(e) => fail(e) as c_int,
    }
}

unsafe fn put_int(val: *mut c_void, len: *mut socklen_t, v: c_int) {
    if !val.is_null() && !len.is_null() && *len as usize >= size_of::<c_int>() {
        *(val as *mut c_int) = v;
        *len = size_of::<c_int>() as socklen_t;
    }
}

#[no_mangle]
pub unsafe extern "C" fn getsockopt(
    fd: c_int,
    level: c_int,
    name: c_int,
    val: *mut c_void,
    len: *mut socklen_t,
) -> c_int {
    let Some((mut g, me)) = sim_fd(fd) else {
        return libc_ret(syscall6(
            libc::SYS_getsockopt,
            fd as i64,
            level as i64,
            name as i64,
            val as i64,
            len as i64,
            0,
        )) as c_int;
    };
    let w = w(&mut g);
    let Some(id) = obj_of(w, fd) else {
        return libc_ret(syscall6(
            libc::SYS_getsockopt,
            fd as i64,
            level as i64,
            name as i64,
            val as i64,
            len as i64,
            0,
        )) as c_int;
    };
    let Some(s) = w.net.sock_mut(id) else {
        return fail(libc::ENOTSOCK) as c_int;
    };
    let v = match (level, name) {
        (libc::SOL_SOCKET, libc::SO_ERROR) => {
            let e = std::mem::take(&mut s.so_error);
            w.trace(
                me,
                format_args!(
                    "so_error o{id} -> {}",
                    if e == 0 { "0".into() } else { errname(e) }
                ),
            );
            e
        }
        (libc::SOL_SOCKET, libc::SO_TYPE) => {
            if s.stream {
                libc::SOCK_STREAM
            } else {
                libc::SOCK_DGRAM
            }
        }
        (libc::SOL_SOCKET, libc::SO_DOMAIN) => s.domain,
        (libc::SOL_SOCKET, libc::SO_REUSEADDR) => s.reuse_addr as c_int,
        (libc::SOL_SOCKET, libc::SO_REUSEPORT) => s.reuse_port as c_int,
        (libc::SOL_SOCKET, libc::SO_ACCEPTCONN) => {
            matches!(s.state, State::Listening { .. }) as c_int
        }
        (libc::SOL_SOCKET, libc::SO_RCVBUF | libc::SO_SNDBUF) => 256 * 1024,
        (libc::IPPROTO_TCP, libc::TCP_NODELAY) => s.nodelay as c_int,
        (libc::IPPROTO_IPV6, libc::IPV6_V6ONLY) => s.v6only as c_int,
        _ => 0,
    };
    put_int(val, len, v);
    0
}

#[no_mangle]
pub unsafe extern "C" fn setsockopt(
    fd: c_int,
    level: c_int,
    name: c_int,
    val: *const c_void,
    len: socklen_t,
) -> c_int {
    let Some((mut g, me)) = sim_fd(fd) else {
        return libc_ret(syscall6(
            libc::SYS_setsockopt,
            fd as i64,
            level as i64,
            name as i64,
            val as i64,
            len as i64,
            0,
        )) as c_int;
    };
    let w = w(&mut g);
    let Some(id) = obj_of(w, fd) else {
        return libc_ret(syscall6(
            libc::SYS_setsockopt,
            fd as i64,
            level as i64,
            name as i64,
            val as i64,
            len as i64,
            0,
        )) as c_int;
    };
    let on = !val.is_null() && len as usize >= size_of::<c_int>() && *(val as *const c_int) != 0;
    let Some(s) = w.net.sock_mut(id) else {
        return fail(libc::ENOTSOCK) as c_int;
    };
    let r = match (level, name) {
        (libc::SOL_SOCKET, libc::SO_REUSEADDR) => {
            s.reuse_addr = on;
            0
        }
        (libc::SOL_SOCKET, libc::SO_REUSEPORT) => {
            s.reuse_port = on;
            0
        }
        (libc::IPPROTO_TCP, libc::TCP_NODELAY) => {
            s.nodelay = on;
            0
        }
        (libc::IPPROTO_IPV6, libc::IPV6_V6ONLY) => {
            s.v6only = on;
            0
        }
        // No kernel TLS in the simulated kernel: the module is "not loaded".
        (libc::IPPROTO_TCP, libc::TCP_ULP) => -libc::ENOENT,
        // UDP segmentation offload is not modelled.
        (libc::SOL_UDP, 103 /* UDP_SEGMENT */ | 104 /* UDP_GRO */) => -libc::ENOPROTOOPT,
        _ => 0,
    };
    w.trace(me, format_args!("setsockopt o{id} {level}/{name} -> {r}"));
    if r < 0 {
        return fail(-r) as c_int;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn shutdown(fd: c_int, how: c_int) -> c_int {
    let Some((mut g, me)) = sim_fd(fd) else {
        return libc_ret(syscall3(libc::SYS_shutdown, fd as i64, how as i64, 0)) as c_int;
    };
    let w = w(&mut g);
    let Some(id) = obj_of(w, fd) else {
        return libc_ret(syscall3(libc::SYS_shutdown, fd as i64, how as i64, 0)) as c_int;
    };
    let r = w.shutdown(id, how).map(|_| 0);
    w.trace(me, format_args!("shutdown o{id} {how} -> {}", res_str(&r)));
    ret(r) as c_int
}

// --------------------------------------------------------- send and receive

/// Sends `data` on object `id` (stream or datagram).
fn do_send(
    g: Guard,
    me: Tid,
    id: ObjId,
    data: &[u8],
    to: Option<Addr>,
    flags: c_int,
) -> (Guard, Result<usize, i32>) {
    let mut g = g;
    if let Some(e) = injected_error(w(&mut g), id, false, flags) {
        w(&mut g).trace(
            me,
            format_args!("send o{id} {} -> {} (injected)", data.len(), errname(e)),
        );
        return (g, Err(e));
    }
    let stream = w(&mut g).net.sock(id).is_some_and(|s| s.stream);
    let dontwait = flags & libc::MSG_DONTWAIT != 0;
    let (mut g, r) = if stream {
        blocking(g, me, id, net::OUT, dontwait, |w| w.send_stream(id, data))
    } else {
        let to2 = to.clone();
        blocking(g, me, id, net::OUT, dontwait, move |w| {
            w.send_dgram(id, to2.clone(), data)
        })
    };
    w(&mut g).trace(
        me,
        format_args!("send o{id} {} -> {}", data.len(), res_str(&r)),
    );
    (g, r)
}

/// Receives on a stream into `buf`.
fn do_recv(
    g: Guard,
    me: Tid,
    id: ObjId,
    buf: &mut [u8],
    flags: c_int,
) -> (Guard, Result<usize, i32>) {
    let mut g = g;
    if let Some(e) = injected_error(w(&mut g), id, true, flags) {
        w(&mut g).trace(me, format_args!("recv o{id} -> {} (injected)", errname(e)));
        return (g, Err(e));
    }
    let dontwait = flags & libc::MSG_DONTWAIT != 0;
    let peek = flags & libc::MSG_PEEK != 0;
    let (mut g, r) = blocking(g, me, id, net::IN, dontwait, |w| {
        w.recv_stream(id, buf, peek)
    });
    w(&mut g).trace(
        me,
        format_args!("recv o{id} {} -> {}", buf.len(), res_str(&r)),
    );
    (g, r)
}

/// Receives one datagram: (source, bytes copied, real length).
fn do_recv_dgram(
    g: Guard,
    me: Tid,
    id: ObjId,
    bufs: &mut [&mut [u8]],
    flags: c_int,
) -> (Guard, Result<(Addr, usize, usize), i32>) {
    let mut g = g;
    if let Some(e) = injected_error(w(&mut g), id, true, flags) {
        return (g, Err(e));
    }
    let dontwait = flags & libc::MSG_DONTWAIT != 0;
    let peek = flags & libc::MSG_PEEK != 0;
    let (mut g, r) = blocking(g, me, id, net::IN, dontwait, |w| w.recv_dgram(id, peek));
    let r = r.map(|(from, data)| {
        let mut off = 0;
        for b in bufs.iter_mut() {
            let n = b.len().min(data.len() - off);
            b[..n].copy_from_slice(&data[off..off + n]);
            off += n;
            if off == data.len() {
                break;
            }
        }
        (from, off, data.len())
    });
    let s = match &r {
        Ok((from, n, len)) => format!("{n}/{len} from {from}"),
        Err(e) => errname(*e),
    };
    w(&mut g).trace(me, format_args!("recvfrom o{id} -> {s}"));
    (g, r)
}

#[no_mangle]
pub unsafe extern "C" fn send(fd: c_int, buf: *const c_void, len: size_t, flags: c_int) -> ssize_t {
    sendto(fd, buf, len, flags, std::ptr::null(), 0)
}

#[no_mangle]
pub unsafe extern "C" fn sendto(
    fd: c_int,
    buf: *const c_void,
    len: size_t,
    flags: c_int,
    addr: *const sockaddr,
    alen: socklen_t,
) -> ssize_t {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_sendto,
            fd as i64,
            buf as i64,
            len as i64,
            flags as i64,
            addr as i64,
            alen as i64,
        )) as ssize_t
    };
    let Some((mut g, me)) = sim_fd(fd) else {
        return real();
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        drop(g);
        return real();
    };
    let to = if addr.is_null() {
        None
    } else {
        match read_addr(addr, alen) {
            Ok(a) => Some(a),
            Err(e) => return fail(e) as ssize_t,
        }
    };
    let data = if len == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(buf as *const u8, len)
    };
    let (_g, r) = do_send(g, me, id, data, to, flags);
    ret(r)
}

#[no_mangle]
pub unsafe extern "C" fn recv(fd: c_int, buf: *mut c_void, len: size_t, flags: c_int) -> ssize_t {
    recvfrom(
        fd,
        buf,
        len,
        flags,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
    )
}

#[no_mangle]
pub unsafe extern "C" fn recvfrom(
    fd: c_int,
    buf: *mut c_void,
    len: size_t,
    flags: c_int,
    addr: *mut sockaddr,
    alen: *mut socklen_t,
) -> ssize_t {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_recvfrom,
            fd as i64,
            buf as i64,
            len as i64,
            flags as i64,
            addr as i64,
            alen as i64,
        )) as ssize_t
    };
    let Some((mut g, me)) = sim_fd(fd) else {
        return real();
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        drop(g);
        return real();
    };
    let slice = if len == 0 {
        &mut [][..]
    } else {
        std::slice::from_raw_parts_mut(buf as *mut u8, len)
    };
    let wr = w(&mut g);
    let (stream, domain) = wr.net.sock(id).map_or((true, 0), |s| (s.stream, s.domain));
    if stream {
        let (mut g, r) = do_recv(g, me, id, slice, flags);
        if r.is_ok() && !addr.is_null() {
            if let Ok(p) = w(&mut g).peer_addr(id) {
                write_addr(&p, domain, addr, alen);
            }
        }
        return ret(r);
    }
    let (_g, r) = do_recv_dgram(g, me, id, &mut [slice], flags);
    match r {
        Ok((from, n, real_len)) => {
            write_addr(&from, domain, addr, alen);
            (if flags & libc::MSG_TRUNC != 0 {
                real_len
            } else {
                n
            }) as ssize_t
        }
        Err(e) => fail(e) as ssize_t,
    }
}

unsafe fn iovecs<'a>(iov: *const libc::iovec, n: usize) -> Vec<&'a mut [u8]> {
    (0..n)
        .map(|i| {
            let v = &*iov.add(i);
            if v.iov_len == 0 {
                &mut [][..]
            } else {
                std::slice::from_raw_parts_mut(v.iov_base as *mut u8, v.iov_len)
            }
        })
        .collect()
}

#[no_mangle]
pub unsafe extern "C" fn sendmsg(fd: c_int, msg: *const libc::msghdr, flags: c_int) -> ssize_t {
    let real = || {
        libc_ret(syscall3(
            libc::SYS_sendmsg,
            fd as i64,
            msg as i64,
            flags as i64,
        )) as ssize_t
    };
    let Some((mut g, me)) = sim_fd(fd) else {
        return real();
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        drop(g);
        return real();
    };
    let m = &*msg;
    if m.msg_controllen != 0 {
        // Control messages (kernel TLS records, GSO) are not modelled.
        w(&mut g).trace(me, format_args!("sendmsg o{id} with control -> EINVAL"));
        return fail(libc::EINVAL) as ssize_t;
    }
    let data: Vec<u8> = iovecs(m.msg_iov, m.msg_iovlen).concat();
    let to = if m.msg_name.is_null() {
        None
    } else {
        match read_addr(m.msg_name as *const sockaddr, m.msg_namelen) {
            Ok(a) => Some(a),
            Err(e) => return fail(e) as ssize_t,
        }
    };
    let (_g, r) = do_send(g, me, id, &data, to, flags);
    ret(r)
}

#[no_mangle]
pub unsafe extern "C" fn recvmsg(fd: c_int, msg: *mut libc::msghdr, flags: c_int) -> ssize_t {
    let real = || {
        libc_ret(syscall3(
            libc::SYS_recvmsg,
            fd as i64,
            msg as i64,
            flags as i64,
        )) as ssize_t
    };
    let Some((mut g, me)) = sim_fd(fd) else {
        return real();
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        drop(g);
        return real();
    };
    let m = &mut *msg;
    let mut bufs = iovecs(m.msg_iov, m.msg_iovlen);
    let (stream, domain) = w(&mut g)
        .net
        .sock(id)
        .map_or((true, 0), |s| (s.stream, s.domain));
    m.msg_controllen = 0;
    m.msg_flags = 0;
    if stream {
        let total: usize = bufs.iter().map(|b| b.len()).sum();
        let mut tmp = vec![0u8; total];
        let (_g, r) = do_recv(g, me, id, &mut tmp, flags);
        if let Ok(n) = r {
            let mut off = 0;
            for b in bufs.iter_mut() {
                let k = b.len().min(n - off);
                b[..k].copy_from_slice(&tmp[off..off + k]);
                off += k;
            }
        }
        m.msg_namelen = 0;
        return ret(r);
    }
    let (_g, r) = do_recv_dgram(g, me, id, &mut bufs, flags);
    match r {
        Ok((from, n, real_len)) => {
            if !m.msg_name.is_null() {
                let mut l = m.msg_namelen;
                write_addr(&from, domain, m.msg_name as *mut sockaddr, &mut l);
                m.msg_namelen = l;
            }
            if real_len > n {
                m.msg_flags |= libc::MSG_TRUNC;
            }
            (if flags & libc::MSG_TRUNC != 0 {
                real_len
            } else {
                n
            }) as ssize_t
        }
        Err(e) => fail(e) as ssize_t,
    }
}

/// `recvmmsg(2)`: one simulated `recvmsg` per header, the first with the
/// caller's flags and the rest without waiting, as the kernel does; it stops
/// at the first that finds nothing. The timeout is not modelled (Gióng
/// passes none). Descriptors outside the simulation go through `recvmsg`'s
/// own pass-through, one call per header.
#[no_mangle]
pub unsafe extern "C" fn recvmmsg(
    fd: c_int,
    msgs: *mut libc::mmsghdr,
    vlen: c_uint,
    flags: c_int,
    _timeout: *mut libc::timespec,
) -> c_int {
    let mut got: c_uint = 0;
    while got < vlen {
        let m = &mut *msgs.add(got as usize);
        let more = if got == 0 {
            flags
        } else {
            flags | libc::MSG_DONTWAIT
        };
        let r = recvmsg(fd, &mut m.msg_hdr, more & !libc::MSG_WAITFORONE);
        if r < 0 {
            if got == 0 {
                return -1;
            }
            break;
        }
        m.msg_len = r as c_uint;
        got += 1;
    }
    got as c_int
}

/// `sendmmsg(2)`: one simulated `sendmsg` per header, stopping at the first
/// error, which is returned only if nothing was sent, as the kernel does.
#[no_mangle]
pub unsafe extern "C" fn sendmmsg(
    fd: c_int,
    msgs: *mut libc::mmsghdr,
    vlen: c_uint,
    flags: c_int,
) -> c_int {
    let mut sent: c_uint = 0;
    while sent < vlen {
        let m = &mut *msgs.add(sent as usize);
        let r = sendmsg(fd, &m.msg_hdr, flags);
        if r < 0 {
            if sent == 0 {
                return -1;
            }
            break;
        }
        m.msg_len = r as c_uint;
        sent += 1;
    }
    sent as c_int
}

// ------------------------------------------------------- generic descriptors

#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut c_void, len: size_t) -> ssize_t {
    let real = || libc_ret(syscall3(libc::SYS_read, fd as i64, buf as i64, len as i64)) as ssize_t;
    let Some((mut g, me)) = sim_fd(fd) else {
        return real();
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        return files::read(g, me, fd, buf, len);
    };
    if let Some(Kind::EventFd { .. }) = w(&mut g).net.objs.get(&id).map(|o| &o.kind) {
        if len < 8 {
            return fail(libc::EINVAL) as ssize_t;
        }
        let (mut g, r) = blocking(g, me, id, net::IN, false, |w| {
            let Some(Kind::EventFd { count, semaphore }) =
                w.net.objs.get_mut(&id).map(|o| &mut o.kind)
            else {
                return Err(libc::EBADF);
            };
            if *count == 0 {
                return Err(libc::EAGAIN);
            }
            let v = if *semaphore { 1 } else { *count };
            *count -= v;
            Ok(v)
        });
        w(&mut g).trace(
            me,
            format_args!(
                "eventfd read o{id} -> {}",
                r.map_or_else(errname, |v| v.to_string())
            ),
        );
        return match r {
            Ok(v) => {
                *(buf as *mut u64) = v;
                8
            }
            Err(e) => fail(e) as ssize_t,
        };
    }
    recvfrom(fd, buf, len, 0, std::ptr::null_mut(), std::ptr::null_mut())
}

#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const c_void, len: size_t) -> ssize_t {
    let real = || libc_ret(syscall3(libc::SYS_write, fd as i64, buf as i64, len as i64)) as ssize_t;
    // Standard output and error never enter the simulation (test output).
    if fd <= 2 {
        return real();
    }
    let Some((mut g, me)) = sim_fd(fd) else {
        return real();
    };
    let Some(id) = obj_of(w(&mut g), fd) else {
        return files::write(g, me, fd, buf, len);
    };
    if let Some(Kind::EventFd { .. }) = w(&mut g).net.objs.get(&id).map(|o| &o.kind) {
        if len < 8 {
            return fail(libc::EINVAL) as ssize_t;
        }
        let v = *(buf as *const u64);
        let wr = w(&mut g);
        if let Some(Kind::EventFd { count, .. }) = wr.net.objs.get_mut(&id).map(|o| &mut o.kind) {
            *count = count.saturating_add(v);
        }
        wr.net.bump(id);
        wr.trace(me, format_args!("eventfd write o{id} {v}"));
        return 8;
    }
    drop(g);
    sendto(fd, buf, len, 0, std::ptr::null(), 0)
}

#[no_mangle]
pub unsafe extern "C" fn readv(fd: c_int, iov: *const libc::iovec, n: c_int) -> ssize_t {
    let real = || libc_ret(syscall3(libc::SYS_readv, fd as i64, iov as i64, n as i64)) as ssize_t;
    if !world::is_sim_thread() {
        return real();
    }
    let is_virtual = world::enter().is_some_and(|(mut g, _)| obj_of(w(&mut g), fd).is_some());
    if !is_virtual {
        return real();
    }
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = iov as *mut _;
    msg.msg_iovlen = n as _;
    recvmsg(fd, &mut msg, 0)
}

#[no_mangle]
pub unsafe extern "C" fn writev(fd: c_int, iov: *const libc::iovec, n: c_int) -> ssize_t {
    let real = || libc_ret(syscall3(libc::SYS_writev, fd as i64, iov as i64, n as i64)) as ssize_t;
    if fd <= 2 || !world::is_sim_thread() {
        return real();
    }
    let is_virtual = world::enter().is_some_and(|(mut g, _)| obj_of(w(&mut g), fd).is_some());
    if !is_virtual {
        return real();
    }
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = iov as *mut _;
    msg.msg_iovlen = n as _;
    sendmsg(fd, &msg, libc::MSG_NOSIGNAL)
}

#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    let real = || libc_ret(syscall3(libc::SYS_close, fd as i64, 0, 0)) as c_int;
    // A thread outside the simulation (a thread-local destructor after a
    // simulated thread ended) may still close a simulated descriptor.
    let mut g = Guard::lock();
    let Some(wr) = g.as_mut() else {
        drop(g);
        return real();
    };
    let Some(id) = wr.net.fds.remove(&fd) else {
        wr_files_forget(wr, fd);
        drop(g);
        return real();
    };
    let last = match wr.net.objs.get_mut(&id) {
        Some(o) => {
            o.refs -= 1;
            o.refs == 0
        }
        None => true,
    };
    if last {
        wr.destroy(id);
    }
    let tid = wr.current;
    wr.trace(
        tid,
        format_args!("close o{id}{}", if last { "" } else { " (dup)" }),
    );
    drop(g);
    real()
}

fn wr_files_forget(w: &mut World, fd: c_int) {
    w.files.forget(fd);
}

#[no_mangle]
pub unsafe extern "C" fn dup(fd: c_int) -> c_int {
    fcntl(fd, libc::F_DUPFD, 0)
}

#[no_mangle]
pub unsafe extern "C" fn fcntl(fd: c_int, cmd: c_int, arg: c_long) -> c_int {
    let real = || libc_ret(syscall3(libc::SYS_fcntl, fd as i64, cmd as i64, arg)) as c_int;
    let Some((mut g, me)) = sim_fd(fd) else {
        return real();
    };
    let wr = w(&mut g);
    let Some(id) = obj_of(wr, fd) else {
        drop(g);
        return real();
    };
    match cmd {
        libc::F_GETFL => {
            let nb = wr.net.objs.get(&id).is_some_and(|o| o.nonblock);
            libc::O_RDWR | if nb { libc::O_NONBLOCK } else { 0 }
        }
        libc::F_SETFL => {
            if let Some(o) = wr.net.objs.get_mut(&id) {
                o.nonblock = arg as c_int & libc::O_NONBLOCK != 0;
            }
            0
        }
        libc::F_DUPFD | libc::F_DUPFD_CLOEXEC => {
            let nfd = new_fd(wr, id);
            wr.trace(me, format_args!("dup o{id}"));
            nfd
        }
        // Close-on-exec flags and the like: accepted, not modelled.
        _ => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn ioctl(fd: c_int, req: c_long, arg: *mut c_void) -> c_int {
    let real = || libc_ret(syscall3(libc::SYS_ioctl, fd as i64, req, arg as i64)) as c_int;
    let Some((mut g, _me)) = sim_fd(fd) else {
        return real();
    };
    let wr = w(&mut g);
    let Some(id) = obj_of(wr, fd) else {
        drop(g);
        return real();
    };
    if req as libc::c_ulong == libc::FIONBIO as libc::c_ulong {
        let on = *(arg as *const c_int) != 0;
        if let Some(o) = wr.net.objs.get_mut(&id) {
            o.nonblock = on;
        }
        return 0;
    }
    if req as libc::c_ulong == libc::FIONREAD as libc::c_ulong {
        *(arg as *mut c_int) = 0;
        return 0;
    }
    fail(libc::ENOTTY) as c_int
}

#[no_mangle]
pub unsafe extern "C" fn fstat64(fd: c_int, st: *mut libc::stat64) -> c_int {
    let real = || libc_ret(syscall3(libc::SYS_fstat, fd as i64, st as i64, 0)) as c_int;
    let Some((mut g, _me)) = sim_fd(fd) else {
        return real();
    };
    let wr = w(&mut g);
    let Some(id) = obj_of(wr, fd) else {
        drop(g);
        return real();
    };
    let r = real();
    if r == 0 {
        let mode = match wr.net.objs.get(&id).map(|o| &o.kind) {
            Some(Kind::Sock(_)) => libc::S_IFSOCK,
            _ => 0, // anonymous inode
        };
        (*st).st_mode = mode | 0o600;
        (*st).st_ino = id;
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn fstat(fd: c_int, st: *mut libc::stat64) -> c_int {
    fstat64(fd, st)
}

// -------------------------------------------------------- epoll and eventfd

#[no_mangle]
pub unsafe extern "C" fn eventfd(init: c_uint, flags: c_int) -> c_int {
    let Some((mut g, me)) = sim() else {
        return libc_ret(syscall3(libc::SYS_eventfd2, init as i64, flags as i64, 0)) as c_int;
    };
    let wr = w(&mut g);
    let id = wr.net.add(
        Kind::EventFd {
            count: u64::from(init),
            semaphore: flags & libc::EFD_SEMAPHORE != 0,
        },
        flags & libc::EFD_NONBLOCK != 0,
    );
    let fd = new_fd(wr, id);
    wr.trace(me, format_args!("eventfd -> o{id}"));
    fd
}

#[no_mangle]
pub unsafe extern "C" fn epoll_create1(flags: c_int) -> c_int {
    let Some((mut g, me)) = sim() else {
        return libc_ret(syscall3(libc::SYS_epoll_create1, flags as i64, 0, 0)) as c_int;
    };
    let wr = w(&mut g);
    let id = wr.net.add(
        Kind::Epoll {
            list: Default::default(),
        },
        false,
    );
    let fd = new_fd(wr, id);
    wr.trace(me, format_args!("epoll_create -> o{id}"));
    fd
}

#[no_mangle]
pub unsafe extern "C" fn epoll_create(_size: c_int) -> c_int {
    epoll_create1(0)
}

#[no_mangle]
pub unsafe extern "C" fn epoll_ctl(
    epfd: c_int,
    op: c_int,
    fd: c_int,
    ev: *mut libc::epoll_event,
) -> c_int {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_epoll_ctl,
            epfd as i64,
            op as i64,
            fd as i64,
            ev as i64,
            0,
            0,
        )) as c_int
    };
    let Some((mut g, me)) = sim_fd(epfd) else {
        return real();
    };
    let wr = w(&mut g);
    let Some(ep) = obj_of(wr, epfd) else {
        drop(g);
        return real();
    };
    let Some(target) = obj_of(wr, fd) else {
        // A real descriptor (a regular file) in a simulated epoll set.
        wr.trace(me, format_args!("epoll_ctl o{ep} real fd -> EPERM"));
        return fail(libc::EPERM) as c_int;
    };
    let (events, data) = if ev.is_null() {
        (0, 0)
    } else {
        ((*ev).events, (*ev).u64)
    };
    let ev_now = wr.net.objs.get(&target).map_or(0, |o| o.ev);
    let Some(Kind::Epoll { list }) = wr.net.objs.get_mut(&ep).map(|o| &mut o.kind) else {
        return fail(libc::EINVAL) as c_int;
    };
    let r = match op {
        libc::EPOLL_CTL_ADD => {
            if let std::collections::btree_map::Entry::Vacant(e) = list.entry(target) {
                e.insert(net::Interest {
                    events,
                    data,
                    seen: ev_now.wrapping_sub(1),
                });
                Ok(0)
            } else {
                Err(libc::EEXIST)
            }
        }
        libc::EPOLL_CTL_MOD => match list.get_mut(&target) {
            Some(i) => {
                *i = net::Interest {
                    events,
                    data,
                    seen: ev_now.wrapping_sub(1),
                };
                Ok(0)
            }
            None => Err(libc::ENOENT),
        },
        libc::EPOLL_CTL_DEL => list.remove(&target).map(|_| 0).ok_or(libc::ENOENT),
        _ => Err(libc::EINVAL),
    };
    wr.trace(
        me,
        format_args!("epoll_ctl o{ep} {op} o{target} -> {}", res_str(&r)),
    );
    ret(r) as c_int
}

/// Waits on a simulated epoll set; `timeout` in nanoseconds, `None` for
/// no limit.
unsafe fn epoll_wait_sim(
    g: Guard,
    me: Tid,
    ep: ObjId,
    out: *mut libc::epoll_event,
    max: c_int,
    timeout: Option<u64>,
) -> c_int {
    let mut g = g;
    let wr = w(&mut g);
    let p = wr.faults.eintr;
    if timeout != Some(0) && wr.fault(p, "eintr") {
        wr.trace(me, format_args!("epoll_wait o{ep} -> EINTR (injected)"));
        return fail(libc::EINTR) as c_int;
    }
    if !wr.net.epoll_has_ready(ep) && timeout != Some(0) {
        let deadline = timeout.map(|t| wr.now + t);
        let (g2, _) = world::block(g, me, Why::Epoll { ep }, deadline);
        g = g2;
        check(&g);
    }
    let wr = w(&mut g);
    let ready = wr.net.epoll_ready(ep);
    let n = ready.len().min(max.max(0) as usize);
    for (i, (id, events, data)) in ready.iter().take(n).enumerate() {
        out.add(i).write(libc::epoll_event {
            events: *events,
            u64: *data,
        });
        wr.net.epoll_seen(ep, *id);
    }
    let ids: Vec<String> = ready
        .iter()
        .take(n)
        .map(|(id, e, _)| format!("o{id}:{e:x}"))
        .collect();
    wr.trace(me, format_args!("epoll_wait o{ep} -> [{}]", ids.join(" ")));
    n as c_int
}

#[no_mangle]
pub unsafe extern "C" fn epoll_wait(
    epfd: c_int,
    out: *mut libc::epoll_event,
    max: c_int,
    ms: c_int,
) -> c_int {
    epoll_pwait(epfd, out, max, ms, std::ptr::null())
}

#[no_mangle]
pub unsafe extern "C" fn epoll_pwait(
    epfd: c_int,
    out: *mut libc::epoll_event,
    max: c_int,
    ms: c_int,
    sigmask: *const libc::sigset_t,
) -> c_int {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_epoll_pwait,
            epfd as i64,
            out as i64,
            max as i64,
            ms as i64,
            sigmask as i64,
            8,
        )) as c_int
    };
    let Some((mut g, me)) = sim_fd(epfd) else {
        return real();
    };
    let Some(ep) = obj_of(w(&mut g), epfd) else {
        drop(g);
        return real();
    };
    let timeout = (ms >= 0).then(|| ms as u64 * 1_000_000);
    epoll_wait_sim(g, me, ep, out, max, timeout)
}

// -------------------------------------------------------------------- time

fn virtual_clock(w: &World, clk: c_int) -> u64 {
    match clk {
        libc::CLOCK_REALTIME | libc::CLOCK_REALTIME_COARSE | libc::CLOCK_TAI => {
            w.now - T0 + REALTIME_AT_T0
        }
        _ => w.now,
    }
}

#[no_mangle]
pub unsafe extern "C" fn clock_gettime(clk: libc::clockid_t, ts: *mut libc::timespec) -> c_int {
    let real = || libc_ret(syscall3(libc::SYS_clock_gettime, clk as i64, ts as i64, 0)) as c_int;
    if matches!(
        clk,
        libc::CLOCK_PROCESS_CPUTIME_ID | libc::CLOCK_THREAD_CPUTIME_ID
    ) {
        return real();
    }
    let Some((mut g, me)) = world::enter() else {
        return real();
    };
    // Another thread's CPU-time clock (from `pthread_getcpuclockid`; dynamic
    // clock ids are negative): not simulated, and the real one would let the
    // host decide a run. Refused, as the kernel refuses the clock of a thread
    // that has ended, so balanced listeners go by counts of tasks here.
    if clk < 0 {
        drop(g);
        return libc_ret(-i64::from(libc::EINVAL)) as c_int;
    }
    // A thread that only spins on the clock (watching a counter another
    // thread changes) gives the others a turn now and then.
    let wr = w(&mut g);
    wr.clock_reads += 1;
    if wr.clock_reads >= 1000 {
        wr.clock_reads = 0;
        wr.threads[me].state = world::TState::Running;
        g = world::switch(g, me);
        check(&g);
    }
    // Reading the clock takes a little time, so that a loop that spins on
    // the clock (and makes no other call) still sees time pass. No random
    // draw and no preemption: a read that happens only in some runs must
    // not change what the seed decides.
    w(&mut g).now += 100;
    let t = virtual_clock(g.as_ref().unwrap(), clk);
    (*ts).tv_sec = (t / 1_000_000_000) as libc::time_t;
    (*ts).tv_nsec = (t % 1_000_000_000) as libc::c_long;
    0
}

#[no_mangle]
pub unsafe extern "C" fn gettimeofday(tv: *mut libc::timeval, tz: *mut c_void) -> c_int {
    let real = || libc_ret(syscall3(libc::SYS_gettimeofday, tv as i64, tz as i64, 0)) as c_int;
    let Some((g, _me)) = world::enter() else {
        return real();
    };
    let t = virtual_clock(g.as_ref().unwrap(), libc::CLOCK_REALTIME);
    if !tv.is_null() {
        (*tv).tv_sec = (t / 1_000_000_000) as libc::time_t;
        (*tv).tv_usec = ((t % 1_000_000_000) / 1000) as libc::suseconds_t;
    }
    0
}

fn ts_ns(ts: *const libc::timespec) -> Option<u64> {
    if ts.is_null() {
        return None;
    }
    // SAFETY: the caller passed a valid timespec.
    let ts = unsafe { &*ts };
    Some(
        (ts.tv_sec.max(0) as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(ts.tv_nsec.max(0) as u64),
    )
}

fn sleep_until(g: Guard, me: Tid, deadline: u64) -> c_int {
    let mut g = g;
    let wr = w(&mut g);
    let d = deadline.saturating_sub(wr.now);
    wr.trace(me, format_args!("sleep {d}"));
    let (g, _) = world::block(g, me, Why::Sleep, Some(deadline));
    check(&g);
    0
}

#[no_mangle]
pub unsafe extern "C" fn nanosleep(req: *const libc::timespec, rem: *mut libc::timespec) -> c_int {
    let real = || libc_ret(syscall3(libc::SYS_nanosleep, req as i64, rem as i64, 0)) as c_int;
    let Some((mut g, me)) = sim() else {
        return real();
    };
    let d = w(&mut g).now + ts_ns(req).unwrap_or(0);
    sleep_until(g, me, d)
}

#[no_mangle]
pub unsafe extern "C" fn clock_nanosleep(
    clk: libc::clockid_t,
    flags: c_int,
    req: *const libc::timespec,
    rem: *mut libc::timespec,
) -> c_int {
    let Some((mut g, me)) = sim() else {
        return -(syscall6(
            libc::SYS_clock_nanosleep,
            clk as i64,
            flags as i64,
            req as i64,
            rem as i64,
            0,
            0,
        ) as c_int)
            .min(0);
    };
    let wr = w(&mut g);
    let t = ts_ns(req).unwrap_or(0);
    let d = if flags & libc::TIMER_ABSTIME != 0 {
        match clk {
            libc::CLOCK_REALTIME => t.saturating_sub(REALTIME_AT_T0) + T0,
            _ => t,
        }
    } else {
        wr.now + t
    };
    sleep_until(g, me, d)
}

#[no_mangle]
pub unsafe extern "C" fn sched_yield() -> c_int {
    let Some((mut g, me)) = sim() else {
        raw::sched_yield();
        return 0;
    };
    w(&mut g).threads[me].state = world::TState::Runnable;
    let g = world::switch(g, me);
    check(&g);
    0
}

#[no_mangle]
pub unsafe extern "C" fn getrandom(buf: *mut c_void, len: size_t, flags: c_uint) -> ssize_t {
    let Some((mut g, _me)) = world::enter() else {
        return libc_ret(syscall3(
            libc::SYS_getrandom,
            buf as i64,
            len as i64,
            flags as i64,
        )) as ssize_t;
    };
    let s = std::slice::from_raw_parts_mut(buf as *mut u8, len);
    w(&mut g).fill_data(s);
    len as ssize_t
}

// ------------------------------------------------------------------ syscall

#[no_mangle]
pub unsafe extern "C" fn syscall(
    n: c_long,
    a1: c_long,
    a2: c_long,
    a3: c_long,
    a4: c_long,
    a5: c_long,
    a6: c_long,
) -> c_long {
    let real = || libc_ret(syscall6(n, a1, a2, a3, a4, a5, a6));
    match n {
        libc::SYS_futex => futex(a1, a2, a3, a4, a6, real),
        libc::SYS_getrandom => getrandom(a1 as *mut c_void, a2 as size_t, a3 as c_uint) as c_long,
        libc::SYS_epoll_pwait2 => {
            let Some((mut g, me)) = sim_fd(a1 as c_int) else {
                return real();
            };
            let Some(ep) = obj_of(w(&mut g), a1 as c_int) else {
                drop(g);
                return real();
            };
            let timeout = ts_ns(a4 as *const libc::timespec);
            epoll_wait_sim(
                g,
                me,
                ep,
                a2 as *mut libc::epoll_event,
                a3 as c_int,
                timeout,
            ) as c_long
        }
        // No io_uring in the simulated kernel: Gióng falls back to epoll.
        libc::SYS_io_uring_setup | libc::SYS_io_uring_enter | libc::SYS_io_uring_register
            if world::is_sim_thread() =>
        {
            fail(libc::ENOSYS)
        }
        libc::SYS_statx => {
            if world::is_sim_thread() {
                statx(
                    a1 as c_int,
                    a2 as *const c_char,
                    a3 as c_int,
                    a4 as c_uint,
                    a5 as *mut libc::statx,
                ) as c_long
            } else {
                real()
            }
        }
        _ => real(),
    }
}

fn futex(
    addr: c_long,
    op: c_long,
    val: c_long,
    timeout: c_long,
    bitset: c_long,
    real: impl Fn() -> c_long,
) -> c_long {
    let Some((mut g, me)) = sim() else {
        return real();
    };
    let cmd = (op as c_int) & libc::FUTEX_CMD_MASK;
    match cmd {
        libc::FUTEX_WAIT | libc::FUTEX_WAIT_BITSET => {
            let t = ts_ns(timeout as *const libc::timespec);
            let wr = w(&mut g);
            let deadline = match (cmd, t) {
                (_, None) => None,
                (libc::FUTEX_WAIT, Some(rel)) => Some(wr.now + rel),
                (_, Some(abs)) => Some(if op as c_int & libc::FUTEX_CLOCK_REALTIME != 0 {
                    abs.saturating_sub(REALTIME_AT_T0) + T0
                } else {
                    abs
                }),
            };
            let bits = if cmd == libc::FUTEX_WAIT {
                u32::MAX
            } else {
                bitset as u32
            };
            let (g, r) = world::futex_wait(g, me, addr as usize, val as u32, bits, deadline);
            check(&g);
            libc_ret(r)
        }
        libc::FUTEX_WAKE | libc::FUTEX_WAKE_BITSET => {
            let bits = if cmd == libc::FUTEX_WAKE {
                u32::MAX
            } else {
                bitset as u32
            };
            world::futex_wake(w(&mut g), addr as usize, val, bits)
        }
        _ => {
            let wr = w(&mut g);
            wr.fail_run(format!("futex operation {cmd} is not simulated"));
            drop(g);
            abort_failed();
        }
    }
}

// ------------------------------------------------------------------ threads

type StartFn = extern "C" fn(*mut c_void) -> *mut c_void;
type CreateFn = unsafe extern "C" fn(
    *mut libc::pthread_t,
    *const libc::pthread_attr_t,
    StartFn,
    *mut c_void,
) -> c_int;
type JoinFn = unsafe extern "C" fn(libc::pthread_t, *mut *mut c_void) -> c_int;

fn real_fn<T: Copy>(cell: &OnceLock<usize>, name: &CStr) -> T {
    let p = *cell.get_or_init(|| {
        // SAFETY: dlsym with a valid name; RTLD_NEXT finds libc's symbol.
        let p = unsafe { libc::dlsym(libc::RTLD_NEXT, name.as_ptr()) } as usize;
        assert!(p != 0, "giong-sim: no real {name:?}");
        p
    });
    // SAFETY: T is the function pointer type of the symbol.
    unsafe { std::mem::transmute_copy(&p) }
}

static REAL_CREATE: OnceLock<usize> = OnceLock::new();
static REAL_JOIN: OnceLock<usize> = OnceLock::new();

struct Start {
    f: StartFn,
    arg: *mut c_void,
    tid: Tid,
    gen: u64,
}

extern "C" fn trampoline(p: *mut c_void) -> *mut c_void {
    // SAFETY: made by pthread_create below.
    let s = unsafe { Box::from_raw(p as *mut Start) };
    let go = {
        let g = Guard::lock();
        match &*g {
            Some(w) if w.gen == s.gen => Some(w.threads[s.tid].go),
            _ => None,
        }
    };
    if let Some(go) = go {
        world::set_me(s.gen, Some(s.tid));
        world::wait_go(go);
    }
    let out = (s.f)(s.arg);
    if let Some((g, me)) = world::enter() {
        world::set_me(0, None);
        world::exit_thread(g, me);
    }
    out
}

#[no_mangle]
pub unsafe extern "C" fn pthread_create(
    th: *mut libc::pthread_t,
    attr: *const libc::pthread_attr_t,
    f: StartFn,
    arg: *mut c_void,
) -> c_int {
    let create: CreateFn = real_fn(&REAL_CREATE, c"pthread_create");
    let Some((mut g, me)) = sim() else {
        return create(th, attr, f, arg);
    };
    let wr = w(&mut g);
    let tid = wr.new_thread(world::TState::Runnable);
    let gen = wr.gen;
    wr.trace(me, format_args!("spawn t{tid}"));
    drop(g);
    let start = Box::into_raw(Box::new(Start { f, arg, tid, gen }));
    let r = create(th, attr, trampoline, start as *mut c_void);
    let mut g = Guard::lock();
    if let Some(wr) = g.as_mut() {
        if r != 0 {
            wr.threads[tid].state = world::TState::Exited;
        } else {
            wr.threads[tid].pthread = Some(*th);
            wr.by_pthread.insert(*th, tid);
        }
    }
    r
}

#[no_mangle]
pub unsafe extern "C" fn pthread_join(th: libc::pthread_t, out: *mut *mut c_void) -> c_int {
    let join: JoinFn = real_fn(&REAL_JOIN, c"pthread_join");
    if let Some((mut g, me)) = sim() {
        if let Some(&tid) = w(&mut g).by_pthread.get(&th) {
            w(&mut g).trace(me, format_args!("join t{tid}"));
            let (g, _) = world::block(g, me, Why::Join { tid }, None);
            check(&g);
        }
    }
    join(th, out)
}

// -------------------------------------------------------------------- files

#[no_mangle]
pub unsafe extern "C" fn openat(
    dir: c_int,
    path: *const c_char,
    flags: c_int,
    mode: c_uint,
) -> c_int {
    files::openat(dir, path, flags, mode)
}

#[no_mangle]
pub unsafe extern "C" fn pread64(
    fd: c_int,
    buf: *mut c_void,
    len: size_t,
    off: libc::off64_t,
) -> ssize_t {
    files::pread(fd, buf, len, off)
}

#[no_mangle]
pub unsafe extern "C" fn preadv2(
    fd: c_int,
    iov: *const libc::iovec,
    count: c_int,
    off: libc::off_t,
    flags: c_int,
) -> ssize_t {
    files::preadv2(fd, iov, count, off, flags)
}

#[no_mangle]
pub unsafe extern "C" fn pwrite64(
    fd: c_int,
    buf: *const c_void,
    len: size_t,
    off: libc::off64_t,
) -> ssize_t {
    files::pwrite(fd, buf, len, off)
}

#[no_mangle]
pub unsafe extern "C" fn fsync(fd: c_int) -> c_int {
    files::sync(fd, false)
}

#[no_mangle]
pub unsafe extern "C" fn fdatasync(fd: c_int) -> c_int {
    files::sync(fd, true)
}

#[no_mangle]
pub unsafe extern "C" fn fallocate64(
    fd: c_int,
    mode: c_int,
    off: libc::off64_t,
    len: libc::off64_t,
) -> c_int {
    files::fallocate(fd, mode, off, len)
}

#[no_mangle]
pub unsafe extern "C" fn statx(
    dir: c_int,
    path: *const c_char,
    flags: c_int,
    mask: c_uint,
    out: *mut libc::statx,
) -> c_int {
    files::statx(dir, path, flags, mask, out)
}

#[no_mangle]
pub unsafe extern "C" fn sendfile64(
    out: c_int,
    input: c_int,
    off: *mut libc::off64_t,
    count: size_t,
) -> ssize_t {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_sendfile,
            out as i64,
            input as i64,
            off as i64,
            count as i64,
            0,
            0,
        )) as ssize_t
    };
    let Some((mut g, me)) = sim_fd(out) else {
        return real();
    };
    let Some(id) = obj_of(w(&mut g), out) else {
        drop(g);
        return real();
    };
    // Read what the socket can take from the (real) file, then send it.
    let room = 256 * 1024;
    let mut buf = vec![0u8; count.min(room)];
    let n = if off.is_null() {
        syscall3(
            libc::SYS_read,
            input as i64,
            buf.as_mut_ptr() as i64,
            buf.len() as i64,
        )
    } else {
        syscall6(
            libc::SYS_pread64,
            input as i64,
            buf.as_mut_ptr() as i64,
            buf.len() as i64,
            *off,
            0,
            0,
        )
    };
    if n < 0 {
        return fail(-n as i32) as ssize_t;
    }
    buf.truncate(n as usize);
    if buf.is_empty() {
        return 0;
    }
    let (_g, r) = do_send(g, me, id, &buf, None, libc::MSG_NOSIGNAL);
    if let Ok(sent) = r {
        if off.is_null() {
            // Give back what was read but not sent.
            let back = n - sent as i64;
            if back > 0 {
                syscall3(libc::SYS_lseek, input as i64, -back, libc::SEEK_CUR as i64);
            }
        } else {
            *off += sent as i64;
        }
    }
    ret(r)
}

#[no_mangle]
pub unsafe extern "C" fn sendfile(
    out: c_int,
    input: c_int,
    off: *mut libc::off64_t,
    count: size_t,
) -> ssize_t {
    sendfile64(out, input, off, count)
}
