//! What Linux and macOS do the same way: the types operations are written
//! in, the wake-up bell's shape, error numbers, the memory of buffer pools.
//! `linux.rs` and `macos.rs` hold only what differs between the two.

use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use crate::error::Code;

/// A socket, as operations name it (`Io`, `MultiIo`): a descriptor on Unix,
/// a `SOCKET` on Windows.
pub(crate) type RawSock = RawFd;

/// A file, pipe or device, as operations name it: a descriptor on Unix, a
/// `HANDLE` on Windows. On Unix it is the same type as [`RawSock`], and
/// code may pass one for the other; on Windows the two are closed, read
/// and written by different calls, so an operation says which it takes.
pub(crate) type RawFile = RawFd;

/// A socket address as the system reads it (`SOCKADDR` on Windows).
pub(crate) type SockAddr = libc::sockaddr;

/// The length of a socket address (`i32` on Windows).
pub(crate) type SockLen = libc::socklen_t;

/// Room for any socket address (`SOCKADDR_STORAGE` on Windows).
#[allow(dead_code)] // The Unix drivers still name `libc::sockaddr_storage`.
pub(crate) type SockAddrStorage = libc::sockaddr_storage;

/// The header of a `sendmsg` or `recvmsg`: buffers, address and control
/// data (`WSAMSG` on Windows, whose fields have other names and order, so
/// code that fills one in is per system).
pub(crate) type MsgHdr = libc::msghdr;

/// One unit of a path as the system takes it: a byte of a NUL-terminated
/// string on Unix, a UTF-16 unit on Windows.
pub(crate) type PathChar = libc::c_char;

/// `-errno` of the last failed call.
pub(crate) fn neg_errno() -> i32 {
    -io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// The `S_IFMT` bits and file types of `st_mode`, as `u32` everywhere
/// (`mode_t` is `u32` on Linux and `u16` on macOS).
#[allow(clippy::unnecessary_cast)]
pub(crate) mod mode {
    pub(crate) const S_IFMT: u32 = libc::S_IFMT as u32;
    pub(crate) const S_IFREG: u32 = libc::S_IFREG as u32;
    pub(crate) const S_IFDIR: u32 = libc::S_IFDIR as u32;
    pub(crate) const S_IFLNK: u32 = libc::S_IFLNK as u32;
    pub(crate) const S_IFIFO: u32 = libc::S_IFIFO as u32;
    pub(crate) const S_IFSOCK: u32 = libc::S_IFSOCK as u32;
    pub(crate) const S_IFCHR: u32 = libc::S_IFCHR as u32;
    pub(crate) const S_IFBLK: u32 = libc::S_IFBLK as u32;
}

/// A descriptor that becomes readable when another thread, or a signal
/// handler, rings it: the blocking pool's completions and the shutdown
/// signals use one. An eventfd on Linux (one descriptor, read and written);
/// a non-blocking pipe elsewhere (read end watched, write end rung).
pub(crate) struct Bell {
    pub(super) read: std::os::fd::OwnedFd,
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    pub(super) write: std::os::fd::OwnedFd,
}

impl Bell {
    /// The descriptor to watch for readability.
    pub(crate) fn read_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        self.read.as_raw_fd()
    }

    /// The descriptor [`ring`] writes to.
    pub(crate) fn write_fd(&self) -> RawFd {
        use std::os::fd::AsRawFd;
        #[cfg(all(target_os = "linux", not(giong_libkqueue)))]
        return self.read.as_raw_fd();
        #[cfg(any(target_os = "macos", giong_libkqueue))]
        return self.write.as_raw_fd();
    }

    /// Rings the bell. Never blocks.
    pub(crate) fn ring(&self) {
        super::ring(self.write_fd());
    }

    /// Resets the bell after it was seen. Never blocks.
    pub(crate) fn drain(&self) {
        super::drain(self.read_fd());
    }

    /// Gives up ownership: the descriptors stay open for ever (the signal
    /// bell, which a handler may write to at any time).
    pub(crate) fn leak(self) -> (RawFd, RawFd) {
        let fds = (self.read_fd(), self.write_fd());
        std::mem::forget(self);
        fds
    }
}

/// Error numbers the code outside the drivers compares results against.
/// On Unix they are `errno` values; on Windows, the Winsock or Win32 codes
/// that mean the same.
pub(crate) mod errno {
    pub(crate) const EADDRINUSE: i32 = libc::EADDRINUSE;
    pub(crate) const EAGAIN: i32 = libc::EAGAIN;
    pub(crate) const EINTR: i32 = libc::EINTR;
    pub(crate) const EIO: i32 = libc::EIO;
    pub(crate) const EOPNOTSUPP: i32 = libc::EOPNOTSUPP;
    /// The same as `EOPNOTSUPP` on Linux; macOS's `F_PREALLOCATE` and
    /// `F_FULLFSYNC` report this one.
    pub(crate) const ENOTSUP: i32 = libc::ENOTSUP;
}

/// Maps `len` bytes of zero-filled, page-aligned memory for a buffer pool.
/// Lazily backed: a page becomes resident when it is first touched. Null
/// if the system refuses; `io::Error::last_os_error` then says why.
pub(crate) fn map_pool(len: usize) -> *mut u8 {
    // SAFETY: a fresh anonymous mapping; the result is checked.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return std::ptr::null_mut();
    }
    ptr.cast()
}

/// Gives back memory from [`map_pool`].
///
/// # Safety
///
/// `ptr` and `len` are exactly one region `map_pool` returned, and nothing
/// uses it any more.
pub(crate) unsafe fn unmap_pool(ptr: *mut u8, len: usize) {
    // SAFETY: the caller hands exactly a region mapped by `map_pool`.
    unsafe { libc::munmap(ptr.cast(), len) };
}

/// The size of a page of memory.
pub(crate) fn page_size() -> usize {
    // SAFETY: plain libc call without pointers.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).unwrap_or(4096)
}

// --------------------------------------------------------------------- signals

/// The bell the signal handler rings (its write descriptor). Created once
/// per process and never closed, so the handler can never race with a close.
static SIGNAL_FD: AtomicI32 = AtomicI32::new(-1);
/// The read descriptor of the same bell (the same eventfd on Linux).
static SIGNAL_READ_FD: AtomicI32 = AtomicI32::new(-1);
/// Set while a runtime has the handlers installed.
static SIGNALS_IN_USE: AtomicBool = AtomicBool::new(false);
/// When the first signal since the handlers were installed arrived, in
/// nanoseconds of `CLOCK_MONOTONIC`; 0 before it.
static FIRST_SIGNAL: AtomicU64 = AtomicU64::new(0);
/// SIGINT, and SIGTERM, were ignored before the handlers were installed
/// (as in a background job of a non-interactive shell).
static INT_IGNORED: AtomicBool = AtomicBool::new(false);
static TERM_IGNORED: AtomicBool = AtomicBool::new(false);

/// A signal this soon after the first is part of the same request: one
/// Ctrl-C reaches every process of the terminal's foreground group, and
/// wrappers (cargo-watch, watchexec, process managers) forward it to their
/// child too, so one key press can deliver two signals. Someone pressing
/// Ctrl-C again because a shutdown hangs is always slower than this.
const SAME_REQUEST_NS: u64 = 250_000_000;

/// `CLOCK_MONOTONIC` in nanoseconds; never 0. Async-signal-safe.
fn monotonic_ns() -> u64 {
    // SAFETY: all-zero is a valid timespec, which the call fills in.
    let mut ts: libc::timespec = unsafe { mem::zeroed() };
    // SAFETY: a valid clock and pointer; `clock_gettime` is
    // async-signal-safe.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    (ts.tv_sec as u64)
        .wrapping_mul(1_000_000_000)
        .wrapping_add(ts.tv_nsec as u64)
        .max(1)
}

/// Async-signal-safe: only `clock_gettime(2)` and `write(2)`, with `errno`
/// preserved, and for a second signal `getpid(2)`, `sigaction(2)` and
/// `raise(3)` or `_exit(2)`.
extern "C" fn on_signal(signal: libc::c_int) {
    let now = monotonic_ns();
    if let Err(first) = FIRST_SIGNAL.compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed)
    {
        if now.saturating_sub(first) < SAME_REQUEST_NS {
            // The same request as the first: its shutdown is under way.
            return;
        }
        let ignored = if signal == libc::SIGINT {
            &INT_IGNORED
        } else {
            &TERM_IGNORED
        };
        if ignored.load(Ordering::Relaxed) {
            // With no handler the signal would have done nothing.
            return;
        }
        // The shutdown the first signal asked for has not finished: do what
        // the signal does with no handler, and end the process. The signal
        // stays blocked while this handler runs, so it is delivered, with
        // its default action, as the handler returns. Process 1 (the
        // program a container runs) is not ended by a signal's default
        // action, so it exits with the status a shell gives a process the
        // signal ended.
        // SAFETY: all four calls are async-signal-safe; the action is valid.
        unsafe {
            if libc::getpid() == 1 {
                libc::_exit(128 + signal);
            }
            let mut action: libc::sigaction = mem::zeroed();
            action.sa_sigaction = libc::SIG_DFL;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
            libc::raise(signal);
        }
        return;
    }
    let fd = SIGNAL_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let errno = super::errno_location();
        // SAFETY: `errno` is the calling thread's, valid for reads and
        // writes; `ring` only calls `write`, which is async-signal-safe.
        unsafe {
            let saved = *errno;
            super::ring(fd);
            *errno = saved;
        }
    }
}

/// Installs SIGINT/SIGTERM handlers that poke an eventfd; restores the
/// previous handlers when dropped.
///
/// Handlers (rather than a blocked-signal `signalfd`) are used because signal
/// masks are per thread: a `signalfd` only works if *every* thread of the
/// process blocks the signals, which a library cannot guarantee.
pub(crate) struct SignalGuard {
    fd: RawFd,
    old_int: libc::sigaction,
    old_term: libc::sigaction,
}

impl SignalGuard {
    pub(crate) fn install() -> io::Result<Self> {
        if SIGNALS_IN_USE.swap(true, Ordering::SeqCst) {
            return Err(crate::error::raise(
                Code::SignalsTaken,
                "another giong runtime in this process already handles SIGINT and SIGTERM",
                "turn on Builder::shutdown_on_signals for one runtime only, or run one runtime \
                 at a time.",
            ));
        }
        // SAFETY: plain libc calls with valid arguments; results are checked.
        unsafe {
            let mut fd = SIGNAL_READ_FD.load(Ordering::SeqCst);
            if fd < 0 {
                let bell = match Bell::new() {
                    Ok(bell) => bell,
                    Err(e) => {
                        SIGNALS_IN_USE.store(false, Ordering::SeqCst);
                        return Err(e);
                    }
                };
                let (read, write) = bell.leak();
                fd = read;
                SIGNAL_READ_FD.store(read, Ordering::SeqCst);
                SIGNAL_FD.store(write, Ordering::SeqCst);
            }
            // Forget signals that arrived while nobody was listening.
            super::drain(fd);
            FIRST_SIGNAL.store(0, Ordering::SeqCst);
            // What each signal did before, read before the handlers go in.
            for (signal, ignored) in [(libc::SIGINT, &INT_IGNORED), (libc::SIGTERM, &TERM_IGNORED)]
            {
                let mut current: libc::sigaction = mem::zeroed();
                let was_ignored = libc::sigaction(signal, std::ptr::null(), &mut current) == 0
                    && current.sa_sigaction == libc::SIG_IGN;
                ignored.store(was_ignored, Ordering::SeqCst);
            }

            let mut action: libc::sigaction = mem::zeroed();
            action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            let mut old_int: libc::sigaction = mem::zeroed();
            let mut old_term: libc::sigaction = mem::zeroed();
            if libc::sigaction(libc::SIGINT, &action, &mut old_int) != 0 {
                let err = io::Error::last_os_error();
                SIGNALS_IN_USE.store(false, Ordering::SeqCst);
                return Err(err);
            }
            if libc::sigaction(libc::SIGTERM, &action, &mut old_term) != 0 {
                let err = io::Error::last_os_error();
                libc::sigaction(libc::SIGINT, &old_int, std::ptr::null_mut());
                SIGNALS_IN_USE.store(false, Ordering::SeqCst);
                return Err(err);
            }
            Ok(Self {
                fd,
                old_int,
                old_term,
            })
        }
    }
}

impl SignalGuard {
    /// The descriptor that becomes readable when a signal arrived.
    pub(crate) fn fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        // SAFETY: restores the dispositions saved in `install`.
        unsafe {
            libc::sigaction(libc::SIGINT, &self.old_int, std::ptr::null_mut());
            libc::sigaction(libc::SIGTERM, &self.old_term, std::ptr::null_mut());
        }
        SIGNALS_IN_USE.store(false, Ordering::SeqCst);
    }
}

/// `MSG_DONTWAIT`: this one call does not wait, whatever the socket's mode.
pub(crate) const MSG_DONTWAIT: i32 = libc::MSG_DONTWAIT;

/// One piece of a vectored send or receive.
pub(crate) type IoVec = libc::iovec;

/// An IPv6 socket address, the largest giong reads from a datagram.
pub(crate) type SockAddrIn6 = libc::sockaddr_in6;

/// The address family of IPv4, as a socket address carries it.
pub(crate) const AF_INET: i32 = libc::AF_INET;
/// The address family of IPv6.
pub(crate) const AF_INET6: i32 = libc::AF_INET6;
/// The flag a received message carries when its end was cut off.
pub(crate) const MSG_TRUNC: i32 = libc::MSG_TRUNC;

/// The piece of `len` bytes at `base`.
#[inline(always)]
pub(crate) fn io_vec(base: *const u8, len: usize) -> IoVec {
    libc::iovec {
        iov_base: base.cast_mut().cast(),
        iov_len: len,
    }
}

/// Where a piece starts and how long it is.
#[inline(always)]
pub(crate) fn io_vec_parts(v: &IoVec) -> (*const u8, usize) {
    (v.iov_base as *const u8, v.iov_len)
}

/// Points a piece at `base`, keeping its length.
#[inline(always)]
pub(crate) fn io_vec_set_base(v: &mut IoVec, base: *const u8) {
    v.iov_base = base.cast_mut().cast();
}

/// The header of a receive that only says how much room the sender's
/// address gets: `name_len` bytes, and nothing else.
pub(crate) const fn recv_header(name_len: usize) -> MsgHdr {
    // SAFETY: all-zero is a valid `msghdr` (null pointers, zero lengths).
    let mut hdr: MsgHdr = unsafe { mem::zeroed() };
    hdr.msg_namelen = name_len as SockLen;
    hdr
}

/// Points a message header at `count` pieces.
///
/// # Safety
///
/// `msg` must point at a live header.
#[inline(always)]
pub(crate) unsafe fn msg_set_iov(msg: *mut MsgHdr, iov: *mut IoVec, count: usize) {
    (*msg).msg_iov = iov;
    (*msg).msg_iovlen = count as _;
}

/// Points a message header at `len` bytes of ancillary data.
///
/// # Safety
///
/// `msg` must point at a live header.
#[inline(always)]
pub(crate) unsafe fn msg_set_control(msg: *mut MsgHdr, control: *mut u8, len: usize) {
    (*msg).msg_control = control.cast();
    (*msg).msg_controllen = len as _;
}

/// Points a message header at an address of `len` bytes, or at room for one.
///
/// # Safety
///
/// `msg` must point at a live header.
#[inline(always)]
pub(crate) unsafe fn msg_set_name(msg: *mut MsgHdr, name: *mut u8, len: SockLen) {
    (*msg).msg_name = name.cast();
    (*msg).msg_namelen = len;
}

/// The length of the address in a message header.
#[inline(always)]
pub(crate) fn msg_name_len(msg: &MsgHdr) -> usize {
    msg.msg_namelen as usize
}

/// `send(2)` that does not wait: the bytes sent, or -1 with `errno` set.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes.
#[inline(always)]
pub(crate) unsafe fn send_nowait(fd: RawSock, buf: *const u8, len: usize) -> isize {
    libc::send(fd, buf.cast(), len, super::MSG_NOSIGNAL | MSG_DONTWAIT)
}

/// `sendto(2)` that does not wait: the bytes sent, or -1 with `errno` set.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes and `addr` for `addr_len`.
#[inline(always)]
pub(crate) unsafe fn send_to_nowait(
    fd: RawSock,
    buf: *const u8,
    len: usize,
    addr: *const SockAddr,
    addr_len: SockLen,
) -> isize {
    libc::sendto(
        fd,
        buf.cast(),
        len,
        super::MSG_NOSIGNAL | MSG_DONTWAIT,
        addr,
        addr_len,
    )
}

/// `true` if a send with `flags` waits for room in the socket's buffer;
/// `false` if it asked not to (`MSG_DONTWAIT`).
#[inline(always)]
pub(crate) fn send_waits(flags: i32) -> bool {
    flags & MSG_DONTWAIT == 0
}
