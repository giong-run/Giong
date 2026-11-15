//! System calls made without going through libc, so that the simulator's own
//! work never re-enters the functions it interposes.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// `syscall` instruction with six arguments; returns `-errno` on failure.
#[inline]
pub(crate) unsafe fn syscall6(n: i64, a1: i64, a2: i64, a3: i64, a4: i64, a5: i64, a6: i64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "syscall",
        inlateout("rax") n => ret,
        in("rdi") a1,
        in("rsi") a2,
        in("rdx") a3,
        in("r10") a4,
        in("r8") a5,
        in("r9") a6,
        lateout("rcx") _,
        lateout("r11") _,
        options(nostack)
    );
    ret
}

pub(crate) unsafe fn syscall3(n: i64, a1: i64, a2: i64, a3: i64) -> i64 {
    syscall6(n, a1, a2, a3, 0, 0, 0)
}

/// Turns a raw result into the libc convention: `-1` and `errno`.
pub(crate) fn libc_ret(ret: i64) -> i64 {
    if (-4095..0).contains(&ret) {
        set_errno(-ret as i32);
        -1
    } else {
        ret
    }
}

pub(crate) fn set_errno(e: i32) {
    // SAFETY: errno is thread-local and always valid.
    unsafe { *libc::__errno_location() = e };
}

/// Fails with `e` the libc way.
pub(crate) fn fail(e: i32) -> i64 {
    set_errno(e);
    -1
}

pub(crate) fn futex_wait(word: &AtomicU32, val: u32) {
    // SAFETY: a valid futex word; FUTEX_WAIT_PRIVATE with no timeout.
    unsafe {
        syscall6(
            libc::SYS_futex,
            word as *const AtomicU32 as i64,
            (libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG) as i64,
            val as i64,
            0,
            0,
            0,
        )
    };
}

pub(crate) fn futex_wake(word: &AtomicU32) {
    // SAFETY: a valid futex word.
    unsafe {
        syscall6(
            libc::SYS_futex,
            word as *const AtomicU32 as i64,
            (libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG) as i64,
            i32::MAX as i64,
            0,
            0,
            0,
        )
    };
}

pub(crate) fn sched_yield() {
    // SAFETY: no arguments.
    unsafe { syscall6(libc::SYS_sched_yield, 0, 0, 0, 0, 0, 0) };
}

/// Writes to standard error without libc (used for reports, which are
/// printed while the simulator's lock is held).
pub(crate) fn eprint(s: &str) {
    let mut b = s.as_bytes();
    while !b.is_empty() {
        // SAFETY: a valid buffer.
        let n = unsafe { syscall3(libc::SYS_write, 2, b.as_ptr() as i64, b.len() as i64) };
        if n <= 0 {
            return;
        }
        b = &b[n as usize..];
    }
}

/// A spin lock: the simulator cannot use `std::sync::Mutex`, whose futex
/// calls it interposes itself. Only one simulated thread runs at a time, so
/// it is rarely contended.
pub(crate) struct SpinLock {
    held: AtomicBool,
}

impl SpinLock {
    pub(crate) const fn new() -> Self {
        Self {
            held: AtomicBool::new(false),
        }
    }

    pub(crate) fn lock(&self) {
        let mut spins = 0u32;
        while self
            .held
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spins += 1;
            if spins > 64 {
                sched_yield();
            } else {
                std::hint::spin_loop();
            }
        }
    }

    pub(crate) fn unlock(&self) {
        self.held.store(false, Ordering::Release);
    }
}

/// Writes `text` to a new file at `path` without libc.
pub(crate) fn write_file(path: &str, text: &str) {
    let Ok(c) = std::ffi::CString::new(path) else {
        return;
    };
    // SAFETY: a valid C string and buffer.
    unsafe {
        let fd = syscall6(
            libc::SYS_openat,
            libc::AT_FDCWD as i64,
            c.as_ptr() as i64,
            (libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC) as i64,
            0o644,
            0,
            0,
        );
        if fd < 0 {
            return;
        }
        let mut b = text.as_bytes();
        while !b.is_empty() {
            let n = syscall3(libc::SYS_write, fd, b.as_ptr() as i64, b.len() as i64);
            if n <= 0 {
                break;
            }
            b = &b[n as usize..];
        }
        syscall3(libc::SYS_close, fd, 0, 0);
    }
}

fn c_path(path: &[u8]) -> Option<std::ffi::CString> {
    std::ffi::CString::new(path.to_vec()).ok()
}

/// `true` if something exists at `path` in the real file system.
pub(crate) fn path_exists(path: &[u8]) -> bool {
    let Some(c) = c_path(path) else { return false };
    // SAFETY: a valid C string; faccessat with F_OK only checks existence.
    unsafe {
        syscall6(
            libc::SYS_faccessat,
            libc::AT_FDCWD as i64,
            c.as_ptr() as i64,
            libc::F_OK as i64,
            0,
            0,
            0,
        ) == 0
    }
}

/// Creates the socket file a Unix socket bound to `path` leaves behind.
pub(crate) fn make_socket_file(path: &[u8]) -> Result<(), i32> {
    let c = c_path(path).ok_or(libc::EINVAL)?;
    // SAFETY: a valid C string.
    let r = unsafe {
        syscall6(
            libc::SYS_mknodat,
            libc::AT_FDCWD as i64,
            c.as_ptr() as i64,
            (libc::S_IFSOCK | 0o755) as i64,
            0,
            0,
            0,
        )
    };
    if r == -(libc::EEXIST as i64) {
        Err(libc::EADDRINUSE)
    } else if r < 0 {
        Err(-r as i32)
    } else {
        Ok(())
    }
}
