//! Files stay real (the simulated process reads and writes the real file
//! system), but the calls Gióng's file operations make (`openat`, `pread64`,
//! `pwrite64`, `fsync`, `fallocate64`, `statx`) can be slowed down in
//! virtual time or made to fail: `EIO`, `ENOSPC`, short reads and writes.
//! Only descriptors opened with `openat` by a simulated thread under
//! [`Faults::disk_prefix`](crate::Faults::disk_prefix) are affected.

use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_uint, c_void, CStr};

use libc::{size_t, ssize_t};

use crate::interpose::errname;
use crate::raw::{fail, libc_ret, syscall3, syscall6};
use crate::world::{self, Guard, Tid, Why};

#[derive(Default)]
pub(crate) struct Files {
    faulty: HashSet<i32>,
    next: u64,
    names: std::collections::HashMap<i32, u64>,
}

impl Files {
    pub(crate) fn forget(&mut self, fd: c_int) {
        self.faulty.remove(&fd);
        self.names.remove(&fd);
    }
}

fn enter() -> Option<(Guard, Tid)> {
    world::enter()
}

/// Waits the disk's latency in virtual time, then says which fault, if any,
/// to inject: `Some(errno)` or `Some(0)` for a short transfer.
fn disk(g: Guard, me: Tid, fd: c_int, what: &str, writes: bool) -> (Guard, Option<i32>, u64) {
    let mut g = g;
    let w = g.as_mut().unwrap();
    if !w.files.faulty.contains(&fd) {
        return (g, None, 0);
    }
    let id = w.files.names.get(&fd).copied().unwrap_or(0);
    let mut g = world::step(g, me);
    let w = g.as_mut().unwrap();
    let (lo, hi) = w.faults.disk_latency_ns;
    let lat = w.between(lo, hi);
    let p = w.faults.disk_slow;
    let lat = if w.fault(p, "disk-slow") {
        lat + w.between(100, 2000) * 1_000_000
    } else {
        lat
    };
    if lat > 0 {
        let deadline = w.now + lat;
        let (g2, _) = world::block(g, me, Why::Sleep, Some(deadline));
        g = g2;
    }
    let w = g.as_mut().unwrap();
    let mut inject = None;
    let p = w.faults.disk_error;
    if w.fault(p, "disk-eio") {
        inject = Some(libc::EIO);
    } else if writes && {
        let p = w.faults.disk_full;
        w.fault(p, "disk-enospc")
    } {
        inject = Some(libc::ENOSPC);
    } else if what != "sync" && {
        let p = w.faults.short_io;
        w.fault(p, "short-file-io")
    } {
        inject = Some(0);
    }
    (g, inject, id)
}

pub(crate) unsafe fn openat(dir: c_int, path: *const c_char, flags: c_int, mode: c_uint) -> c_int {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_openat,
            dir as i64,
            path as i64,
            flags as i64,
            mode as i64,
            0,
            0,
        )) as c_int
    };
    let Some((mut g, me)) = enter() else {
        return real();
    };
    let w = g.as_mut().unwrap();
    let p = CStr::from_ptr(path).to_string_lossy();
    let matches = match &w.faults.disk_prefix {
        Some(prefix) => p.starts_with(prefix.as_str()),
        None => false,
    };
    if !matches {
        drop(g);
        return real();
    }
    let mut g = world::step(g, me);
    let w = g.as_mut().unwrap();
    let pe = w.faults.disk_error;
    if w.fault(pe, "disk-open-eio") {
        w.trace(me, format_args!("openat -> EIO (injected)"));
        return fail(libc::EIO) as c_int;
    }
    let fd = real();
    if fd >= 0 {
        w.files.next += 1;
        let id = w.files.next;
        w.files.faulty.insert(fd);
        w.files.names.insert(fd, id);
        w.trace(me, format_args!("openat -> f{id}"));
    }
    fd
}

pub(crate) unsafe fn read(g: Guard, me: Tid, fd: c_int, buf: *mut c_void, len: size_t) -> ssize_t {
    let (mut g, inject, id) = disk(g, me, fd, "read", false);
    let len = match inject {
        Some(0) if len > 1 => len / 2,
        Some(e) if e != 0 => {
            g.as_mut()
                .unwrap()
                .trace(me, format_args!("read f{id} -> {} (injected)", errname(e)));
            return fail(e) as ssize_t;
        }
        _ => len,
    };
    drop(g);
    libc_ret(syscall3(libc::SYS_read, fd as i64, buf as i64, len as i64)) as ssize_t
}

pub(crate) unsafe fn write(
    g: Guard,
    me: Tid,
    fd: c_int,
    buf: *const c_void,
    len: size_t,
) -> ssize_t {
    let (mut g, inject, id) = disk(g, me, fd, "write", true);
    let len = match inject {
        Some(0) if len > 1 => len / 2,
        Some(e) if e != 0 => {
            g.as_mut()
                .unwrap()
                .trace(me, format_args!("write f{id} -> {} (injected)", errname(e)));
            return fail(e) as ssize_t;
        }
        _ => len,
    };
    drop(g);
    libc_ret(syscall3(libc::SYS_write, fd as i64, buf as i64, len as i64)) as ssize_t
}

pub(crate) unsafe fn pread(fd: c_int, buf: *mut c_void, len: size_t, off: i64) -> ssize_t {
    let real = |len: size_t| {
        libc_ret(syscall6(
            libc::SYS_pread64,
            fd as i64,
            buf as i64,
            len as i64,
            off,
            0,
            0,
        )) as ssize_t
    };
    let Some((g, me)) = enter() else {
        return real(len);
    };
    let (mut g, inject, id) = disk(g, me, fd, "read", false);
    let len = match inject {
        Some(0) if len > 1 => len / 2,
        Some(e) if e != 0 => {
            g.as_mut()
                .unwrap()
                .trace(me, format_args!("pread f{id} -> {} (injected)", errname(e)));
            return fail(e) as ssize_t;
        }
        _ => len,
    };
    let r = real(len);
    if id != 0 {
        g.as_mut()
            .unwrap()
            .trace(me, format_args!("pread f{id} {len} -> {r}"));
    }
    r
}

pub(crate) unsafe fn pwrite(fd: c_int, buf: *const c_void, len: size_t, off: i64) -> ssize_t {
    let real = |len: size_t| {
        libc_ret(syscall6(
            libc::SYS_pwrite64,
            fd as i64,
            buf as i64,
            len as i64,
            off,
            0,
            0,
        )) as ssize_t
    };
    let Some((g, me)) = enter() else {
        return real(len);
    };
    let (mut g, inject, id) = disk(g, me, fd, "write", true);
    let len = match inject {
        Some(0) if len > 1 => len / 2,
        Some(e) if e != 0 => {
            g.as_mut().unwrap().trace(
                me,
                format_args!("pwrite f{id} -> {} (injected)", errname(e)),
            );
            return fail(e) as ssize_t;
        }
        _ => len,
    };
    let r = real(len);
    if id != 0 {
        g.as_mut()
            .unwrap()
            .trace(me, format_args!("pwrite f{id} {len} -> {r}"));
    }
    r
}

pub(crate) unsafe fn sync(fd: c_int, data: bool) -> c_int {
    let n = if data {
        libc::SYS_fdatasync
    } else {
        libc::SYS_fsync
    };
    let real = || libc_ret(syscall3(n, fd as i64, 0, 0)) as c_int;
    let Some((g, me)) = enter() else {
        return real();
    };
    let (mut g, inject, id) = disk(g, me, fd, "sync", true);
    if let Some(e) = inject.filter(|e| *e != 0) {
        g.as_mut()
            .unwrap()
            .trace(me, format_args!("fsync f{id} -> {} (injected)", errname(e)));
        return fail(e) as c_int;
    }
    real()
}

pub(crate) unsafe fn fallocate(fd: c_int, mode: c_int, off: i64, len: i64) -> c_int {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_fallocate,
            fd as i64,
            mode as i64,
            off,
            len,
            0,
            0,
        )) as c_int
    };
    let Some((g, me)) = enter() else {
        return real();
    };
    let (mut g, inject, id) = disk(g, me, fd, "sync", true);
    if let Some(e) = inject.filter(|e| *e != 0) {
        g.as_mut().unwrap().trace(
            me,
            format_args!("fallocate f{id} -> {} (injected)", errname(e)),
        );
        return fail(e) as c_int;
    }
    real()
}

pub(crate) unsafe fn statx(
    dir: c_int,
    path: *const c_char,
    flags: c_int,
    mask: c_uint,
    out: *mut libc::statx,
) -> c_int {
    let real = || {
        libc_ret(syscall6(
            libc::SYS_statx,
            dir as i64,
            path as i64,
            flags as i64,
            mask as i64,
            out as i64,
            0,
        )) as c_int
    };
    let Some((g, me)) = enter() else {
        return real();
    };
    // A statx on a simulated descriptor (an fstat through statx).
    let is_virtual = g.as_ref().unwrap().net.obj(dir).is_some() && !path.is_null() && *path == 0;
    if is_virtual {
        let id = g.as_ref().unwrap().net.obj(dir).unwrap();
        let sock = g.as_ref().unwrap().net.sock(id).is_some();
        drop(g);
        std::ptr::write_bytes(out, 0, 1);
        (*out).stx_mask = libc::STATX_TYPE | libc::STATX_MODE;
        (*out).stx_mode = ((if sock { libc::S_IFSOCK } else { 0 }) | 0o600) as u16;
        (*out).stx_ino = id;
        return 0;
    }
    let (_g, inject, _) = disk(g, me, dir, "sync", false);
    if let Some(e) = inject.filter(|e| *e != 0) {
        return fail(e) as c_int;
    }
    real()
}

impl Files {
    pub(crate) fn open_ids(&self) -> Vec<u64> {
        self.names.values().copied().collect()
    }
}
