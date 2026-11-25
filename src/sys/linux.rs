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
    // `cpu_set_t` holds 1024 CPUs. A kernel built for more possible CPUs
    // refuses a smaller mask with EINVAL, so the mask grows until it fits.
    // The kernel's mask is an array of `unsigned long`.
    const BYTES: usize = mem::size_of::<libc::c_ulong>();
    const BITS: usize = BYTES * 8;
    let mut words = mem::size_of::<libc::cpu_set_t>() / BYTES;
    loop {
        let mut mask: Vec<libc::c_ulong> = vec![0; words];
        // SAFETY: `mask` is `words * BYTES` bytes, aligned like `cpu_set_t`
        // (an array of `unsigned long`), and the kernel writes at most that
        // much.
        let res = unsafe {
            libc::sched_getaffinity(
                0,
                words * BYTES,
                mask.as_mut_ptr().cast::<libc::cpu_set_t>(),
            )
        };
        if res == 0 {
            return (0..words * BITS)
                .filter(|&cpu| mask[cpu / BITS] >> (cpu % BITS) & 1 == 1)
                .collect();
        }
        if io::Error::last_os_error().raw_os_error() != Some(libc::EINVAL) || words >= 1 << 16 {
            let n = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1);
            return (0..n).collect();
        }
        words *= 2;
    }
}

/// `true` where workers can be pinned to a CPU.
pub(crate) const CAN_PIN: bool = true;

/// Pins the calling thread to `cpu`. Failure is not fatal: the worker just
/// stays unpinned.
pub(crate) fn pin_to_cpu(cpu: usize) {
    // At least a `cpu_set_t`, and more for a CPU beyond its 1024. The
    // kernel's mask is an array of `unsigned long`.
    const BYTES: usize = mem::size_of::<libc::c_ulong>();
    const BITS: usize = BYTES * 8;
    let words = (cpu / BITS + 1).max(mem::size_of::<libc::cpu_set_t>() / BYTES);
    let mut mask: Vec<libc::c_ulong> = vec![0; words];
    mask[cpu / BITS] |= 1 << (cpu % BITS);
    // SAFETY: `mask` is `words * BYTES` bytes, aligned like `cpu_set_t`.
    unsafe {
        libc::sched_setaffinity(0, words * BYTES, mask.as_ptr().cast::<libc::cpu_set_t>());
    }
}

/// Fills `buf` from the kernel's random source; `false` if it failed.
pub(crate) fn random_bytes(buf: &mut [u8]) -> bool {
    // SAFETY: the kernel writes at most `buf.len()` bytes into `buf`.
    let n = unsafe { libc::getrandom(buf.as_mut_ptr().cast(), buf.len(), 0) };
    n == buf.len() as isize
}

/// Accepts a connection as a close-on-exec, non-blocking descriptor;
/// -1 with `errno` set on failure (`EINTR` included).
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

/// Whether the directory `dir` is on a filesystem known to answer from local
/// memory or a local disk. `false` for a network or FUSE filesystem (NFS,
/// SMB, 9p, Ceph, FUSE...), for one this list does not know, and when
/// `fstatfs` fails: there a `getdents64` can take a network round trip, or
/// hang with its server.
pub(crate) fn on_local_fs(dir: RawFd) -> bool {
    // SAFETY: all-zero is a valid `statfs`, which the kernel fills in.
    let mut st: libc::statfs = unsafe { mem::zeroed() };
    // SAFETY: a valid pointer to a `statfs`.
    if unsafe { libc::fstatfs(dir, &mut st) } != 0 {
        return false;
    }
    is_local_fs_magic(st.f_type as u64 & 0xFFFF_FFFF)
}

/// The `statfs.f_type` magic numbers (`linux/magic.h`) of local filesystems.
pub(crate) fn is_local_fs_magic(magic: u64) -> bool {
    matches!(
        magic,
        0xEF53 // ext2, ext3, ext4
            | 0x5846_5342 // xfs
            | 0x9123_683E // btrfs
            | 0xF2F5_2010 // f2fs
            | 0xCA45_1A4E // bcachefs
            | 0x2FC1_2FC1 // zfs
            | 0x3153_464A // jfs
            | 0x5265_4973 // reiserfs
            | 0x3434 // nilfs2
            | 0x0102_1994 // tmpfs, devtmpfs
            | 0x8584_58F6 // ramfs
            | 0x9584_58F6 // hugetlbfs
            | 0x794C_7630 // overlayfs
            | 0x7371_7368 // squashfs
            | 0xE0F5_E1E2 // erofs
            | 0x9660 // iso9660
            | 0x1501_3346 // udf
            | 0x4D44 // vfat, msdos
            | 0x2011_BAB0 // exfat
            | 0x7366_746E // ntfs3
            | 0x5346_544E // ntfs
            | 0x482B // hfsplus
            | 0x9FA0 // proc
            | 0x6265_6572 // sysfs
            | 0x6367_7270 // cgroup2
            | 0x0027_E0EB // cgroup
            | 0x1CD1 // devpts
            | 0x6462_6720 // debugfs
            | 0x7472_6163 // tracefs
            | 0x7363_6673 // securityfs
            | 0xCAFE_4A11 // bpf
            | 0x1980_0202 // mqueue
            | 0x6165_676C // pstore
            | 0xDE5E_81E4 // efivarfs
            | 0x6265_6570 // configfs
    )
}

/// What [`read_dir_batch`] keeps between calls: nothing on Linux.
#[derive(Default)]
pub(crate) struct DirState {
    _nothing: (),
}

/// The receive flag that makes the result the datagram's real length, so a
/// datagram cut off by a short buffer is seen as such.
pub(crate) const RECV_REAL_LEN: libc::c_int = libc::MSG_TRUNC;
/// Whether `recvfrom` can tell that a datagram was cut off (through
/// [`RECV_REAL_LEN`]); where it cannot, only `recvmsg`'s flags do.
pub(crate) const RECVFROM_REPORTS_TRUNCATION: bool = true;

/// One entry of [`recvmmsg`].
pub(crate) type MMsgHdr = libc::mmsghdr;

/// Receives up to `count` datagrams: how many, or -1 with `errno` set.
///
/// # Safety
///
/// `msgs` must point at `count` headers whose pointers are valid.
pub(crate) unsafe fn recvmmsg(
    fd: RawFd,
    msgs: *mut MMsgHdr,
    count: usize,
    flags: libc::c_int,
) -> isize {
    // `flags` is an `int` in glibc's prototype and an `unsigned` in musl's.
    libc::recvmmsg(
        fd,
        msgs,
        count as libc::c_uint,
        flags as _,
        std::ptr::null_mut(),
    ) as isize
}

/// The error a [`recvmmsg`] that returned some datagrams met after them,
/// for the caller to report next. Linux keeps such an error on the socket
/// itself (`sk_err`) and returns it from the next receive, so: none.
#[inline(always)]
pub(crate) fn take_recv_error() -> Option<i32> {
    None
}

/// Reads at `offset` what the page cache holds, without waiting for the
/// disk (`preadv2` with `RWF_NOWAIT`): the bytes read, or -1.
///
/// # Safety
///
/// `iov` must describe memory valid for writes.
pub(crate) unsafe fn pread_cached(fd: RawFd, iov: &libc::iovec, offset: u64) -> isize {
    libc::preadv2(fd, iov, 1, offset as libc::off_t, libc::RWF_NOWAIT)
}

/// The `poll` event for "the peer closed its end".
pub(crate) const POLL_PEER_CLOSED: libc::c_short = libc::POLLRDHUP;

/// Sends up to `count` datagrams: how many, or -1 with `errno` set.
///
/// # Safety
///
/// `msgs` must point at `count` headers whose pointers are valid.
pub(crate) unsafe fn sendmmsg(
    fd: RawFd,
    msgs: *mut MMsgHdr,
    count: usize,
    flags: libc::c_int,
) -> isize {
    // `flags`: an `int` in glibc, an `unsigned` in musl.
    libc::sendmmsg(fd, msgs, count as libc::c_uint, flags as _) as isize
}

/// Lowers the calling thread's CPU priority to nice `nice` (compute lane
/// threads). On Linux each thread has its own nice value, and `who == 0` is
/// the calling thread. Raising it needs no privilege; a failure leaves the
/// thread at its old priority, which is safe.
pub(crate) fn lower_thread_priority(nice: i32) {
    // SAFETY: plain syscall with integer arguments.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
}

/// A close-on-exec pipe: `[read, write]`.
pub(crate) fn cloexec_pipe() -> io::Result<[RawFd; 2]> {
    let mut fds = [0; 2];
    // SAFETY: a valid pointer to two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fds)
}

/// The highest signal number, and the first one the C library does not keep
/// for itself among those above the 31 classic ones (`SIGRTMIN`).
pub(crate) fn signal_range() -> (i32, i32) {
    (libc::SIGRTMAX(), libc::SIGRTMIN())
}

#[cfg(test)]
mod tests {
    use super::{allowed_cpus, pin_to_cpu};

    #[test]
    #[cfg_attr(miri, ignore = "sets the thread's CPU affinity")]
    fn pinning_reads_back_and_a_cpu_past_1024_does_not_panic() {
        std::thread::spawn(|| {
            let cpus = allowed_cpus();
            assert!(!cpus.is_empty());
            assert!(cpus.windows(2).all(|w| w[0] < w[1]), "{cpus:?}");
            pin_to_cpu(cpus[cpus.len() - 1]);
            assert_eq!(allowed_cpus(), [cpus[cpus.len() - 1]]);
            // No such CPU here: the kernel refuses, and the thread stays as
            // it was. It used to index past a 1024-CPU mask and panic.
            pin_to_cpu(5000);
            assert_eq!(allowed_cpus(), [cpus[cpus.len() - 1]]);
        })
        .join()
        .unwrap();
    }
}
