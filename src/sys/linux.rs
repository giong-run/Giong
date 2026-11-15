//! Linux: the calls giong has always made, moved here unchanged.

use std::io;
use std::mem;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::Bell;

/// `MSG_NOSIGNAL`: a send to a closed peer fails with `EPIPE` instead of
/// raising `SIGPIPE`.
pub(crate) const MSG_NOSIGNAL: i32 = libc::MSG_NOSIGNAL;

/// The `AT_*` flag that makes a stat of an empty path describe the
/// descriptor itself.
pub(crate) const AT_EMPTY_PATH: i32 = libc::AT_EMPTY_PATH;

/// What a stat writes: `statx` on Linux, the layout io_uring fills in.
pub(crate) type StatBuf = libc::statx;

/// The `statx` mask giong asks for.
pub(crate) const STAT_MASK: u32 =
    libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_SIZE | libc::STATX_MTIME;

/// Size, `st_mode`, and modification time (seconds, nanoseconds).
pub(crate) fn stat_fields(st: &StatBuf) -> (u64, u32, i64, u32) {
    (
        st.stx_size,
        u32::from(st.stx_mode),
        st.stx_mtime.tv_sec,
        st.stx_mtime.tv_nsec,
    )
}

/// `statx(2)`, blocking; `-errno` on failure.
///
/// # Safety
///
/// `path` must be a valid C string and `out` valid for writes.
pub(crate) unsafe fn stat_at(
    dir: RawFd,
    path: *const libc::c_char,
    flags: i32,
    mask: u32,
    out: *mut StatBuf,
) -> isize {
    libc::statx(dir, path, flags, mask, out) as isize
}

impl Bell {
    /// A non-blocking eventfd.
    pub(crate) fn new() -> io::Result<Self> {
        eventfd(libc::EFD_CLOEXEC | libc::EFD_NONBLOCK)
    }

    /// The worker's wake-up eventfd. Blocking (io_uring reads it with a
    /// plain read; the epoll driver makes it non-blocking), except under
    /// Miri, which cannot change its mode later.
    pub(crate) fn new_worker() -> io::Result<Self> {
        #[cfg(not(miri))]
        let flags = libc::EFD_CLOEXEC;
        #[cfg(miri)]
        let flags = libc::EFD_CLOEXEC | libc::EFD_NONBLOCK;
        eventfd(flags)
    }
}

#[cfg(not(giong_libkqueue))]
fn eventfd(flags: i32) -> io::Result<Bell> {
    // SAFETY: plain syscall; the result is checked.
    let fd = unsafe { libc::eventfd(0, flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Bell {
        // SAFETY: a fresh descriptor we own.
        read: unsafe { OwnedFd::from_raw_fd(fd) },
    })
}

/// The test build against libkqueue (`--cfg giong_libkqueue`): libkqueue
/// cannot watch an eventfd, so every bell is a pipe there, as on macOS. Its
/// reads and writes stay 8 bytes, which a pipe carries as well.
#[cfg(giong_libkqueue)]
fn eventfd(flags: i32) -> io::Result<Bell> {
    let mut fds = [0; 2];
    let nonblock = if flags & libc::EFD_NONBLOCK != 0 {
        libc::O_NONBLOCK
    } else {
        0
    };
    // SAFETY: `fds` has room for the two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | nonblock) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // The write end never blocks: a full pipe is already readable.
    // SAFETY: plain fcntl on our descriptor.
    unsafe { libc::fcntl(fds[1], libc::F_SETFL, libc::O_NONBLOCK) };
    // SAFETY: two fresh descriptors we own.
    unsafe {
        Ok(Bell {
            read: OwnedFd::from_raw_fd(fds[0]),
            write: OwnedFd::from_raw_fd(fds[1]),
        })
    }
}

/// Rings a bell by its write descriptor. Async-signal-safe.
pub(crate) fn ring(fd: RawFd) {
    let one: u64 = 1;
    // SAFETY: writes 8 bytes from a valid u64 to an eventfd.
    unsafe { libc::write(fd, &one as *const u64 as *const libc::c_void, 8) };
}

/// Resets a non-blocking bell by its read descriptor.
pub(crate) fn drain(fd: RawFd) {
    let mut count: u64 = 0;
    // SAFETY: reads 8 bytes into a valid u64; a non-blocking eventfd returns
    // EAGAIN at once when it is empty.
    unsafe { libc::read(fd, &mut count as *mut u64 as *mut libc::c_void, 8) };
}

/// The calling thread's `errno`, for a signal handler to save and restore.
pub(crate) fn errno_location() -> *mut i32 {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__errno_location() }
}

/// CPUs the calling thread is allowed to run on.
pub(crate) fn allowed_cpus() -> Vec<usize> {
    // SAFETY: `set` is a valid, zeroed cpu_set_t of the size we pass.
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        if libc::sched_getaffinity(0, mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            let n = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            return (0..n).collect();
        }
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| libc::CPU_ISSET(cpu, &set))
            .collect()
    }
}

/// `true` where workers can be pinned to a CPU.
pub(crate) const CAN_PIN: bool = true;

/// Pins the calling thread to `cpu`. Failure is not fatal: the worker just
/// stays unpinned.
pub(crate) fn pin_to_cpu(cpu: usize) {
    // SAFETY: as above.
    unsafe {
        let mut set: libc::cpu_set_t = mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        libc::sched_setaffinity(0, mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

/// Fills `buf` from the kernel's random source; `false` if it failed.
pub(crate) fn random_bytes(buf: &mut [u8]) -> bool {
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
    n == buf.len() as isize
}

/// Accepts a connection as a close-on-exec, non-blocking descriptor;
/// `-errno` on failure (`EINTR` included).
pub(crate) fn accept(fd: RawFd) -> isize {
    let flags = libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
    // SAFETY: null address pointers ask for no peer address.
    unsafe { libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), flags) as isize }
}

/// `pread(2)`.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes of writes.
pub(crate) unsafe fn pread(fd: RawFd, buf: *mut u8, len: usize, offset: u64) -> isize {
    libc::pread64(fd, buf.cast(), len, offset as i64)
}

/// `pwrite(2)`.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes of reads.
pub(crate) unsafe fn pwrite(fd: RawFd, buf: *const u8, len: usize, offset: u64) -> isize {
    libc::pwrite64(fd, buf.cast(), len, offset as i64)
}

/// `st_mode` of an open descriptor, from `fstat`.
pub(crate) fn fd_mode(fd: RawFd) -> Option<u32> {
    // SAFETY: all-zero is a valid `stat64`, which fstat fills in.
    let mut st: libc::stat64 = unsafe { mem::zeroed() };
    // SAFETY: a valid pointer to a `stat64`.
    (unsafe { libc::fstat64(fd, &mut st) } == 0).then_some(st.st_mode)
}

/// `fsync(2)`, or `fdatasync(2)` when `datasync`.
pub(crate) fn fsync(fd: RawFd, datasync: bool) -> isize {
    // SAFETY: plain syscalls on a descriptor.
    unsafe {
        if datasync {
            libc::fdatasync(fd) as isize
        } else {
            libc::fsync(fd) as isize
        }
    }
}

/// Reserves `len` bytes from `offset` (`fallocate(2)` with mode 0).
pub(crate) fn fallocate(fd: RawFd, offset: u64, len: u64) -> isize {
    // SAFETY: plain syscall on a descriptor.
    unsafe { libc::fallocate64(fd, 0, offset as i64, len as i64) as isize }
}

/// Sends up to `count` bytes of `file` from `*offset` to socket `sock`,
/// advancing `*offset`. Returns the bytes sent, 0 at the end of the file,
/// or -1 with `errno` set.
pub(crate) fn sendfile(sock: RawFd, file: RawFd, offset: &mut u64, count: usize) -> isize {
    let mut off = *offset as libc::off_t;
    // SAFETY: plain system call; `off` is a valid `off_t` the kernel reads
    // and advances.
    let n = unsafe { libc::sendfile64(sock, file, &mut off, count) };
    if n > 0 {
        *offset = off as u64;
    }
    n
}

/// Reads a batch of directory entries from `dir` into `buf` (whose
/// capacity is the batch size) as `linux_dirent64` records: `d_ino: u64,
/// d_off: i64, d_reclen: u16, d_type: u8, d_name` NUL-terminated. Returns
/// the bytes written; 0 at the end.
pub(crate) fn read_dir_batch(
    _state: &mut DirState,
    dir: RawFd,
    buf: &mut Vec<u8>,
) -> io::Result<usize> {
    buf.clear();
    // SAFETY: the kernel writes at most `capacity` bytes into the buffer.
    let n = unsafe { libc::syscall(libc::SYS_getdents64, dir, buf.as_mut_ptr(), buf.capacity()) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel wrote `n` bytes, no more than the capacity.
    unsafe { buf.set_len(n as usize) };
    Ok(n as usize)
}

/// What [`read_dir_batch`] keeps between calls: nothing on Linux.
#[derive(Default)]
pub(crate) struct DirState {
    _nothing: (),
}
