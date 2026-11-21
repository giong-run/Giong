//! macOS: the closest equivalent of each Linux call, with the differences
//! written next to it.

use std::io;
use std::mem;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::Bell;

/// macOS has no `MSG_NOSIGNAL`: the socket carries `SO_NOSIGPIPE` instead,
/// so a send to a closed peer fails with `EPIPE`. socket2 sets it on every
/// socket it creates, accepted sockets inherit it from their listener, and
/// the `from_std` constructors and every listener set it themselves.
pub(crate) const MSG_NOSIGNAL: i32 = 0;

/// giong's own flag for "stat the descriptor itself" (macOS has no
/// `AT_EMPTY_PATH`): [`stat_at`] then calls `fstat`. A bit no `AT_*` flag of
/// macOS uses.
pub(crate) const AT_EMPTY_PATH: i32 = 0x4000_0000;

/// What a stat writes: a plain `struct stat`.
pub(crate) type StatBuf = libc::stat;

/// Unused on macOS: `fstatat` fills in every field.
pub(crate) const STAT_MASK: u32 = 0;

/// Size, `st_mode`, and modification time (seconds, nanoseconds).
pub(crate) fn stat_fields(st: &StatBuf) -> (u64, u32, i64, u32) {
    (
        st.st_size as u64,
        u32::from(st.st_mode),
        st.st_mtime,
        st.st_mtime_nsec as u32,
    )
}

/// `fstatat(2)`, or `fstat(2)` for [`AT_EMPTY_PATH`]; blocking.
///
/// # Safety
///
/// `path` must be a valid C string and `out` valid for writes.
pub(crate) unsafe fn stat_at(
    dir: RawFd,
    path: *const libc::c_char,
    flags: i32,
    _mask: u32,
    out: *mut StatBuf,
) -> isize {
    if flags & AT_EMPTY_PATH != 0 && *path == 0 {
        return libc::fstat(dir, out) as isize;
    }
    libc::fstatat(dir, path, out, flags & !AT_EMPTY_PATH) as isize
}

impl Bell {
    /// A non-blocking, close-on-exec pipe. macOS has no `pipe2`, so the
    /// flags are set after the `pipe`: a `fork` and `exec` on another thread
    /// in between would see the two descriptors (as with `accept` below).
    pub(crate) fn new() -> io::Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: two fresh descriptors we own.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        for fd in fds {
            set_cloexec(fd)?;
            set_nonblocking(fd)?;
        }
        Ok(Bell { read, write })
    }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain fcntl on a descriptor; the result is checked.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let mut on: libc::c_int = 1;
    // SAFETY: FIONBIO reads an int from a valid pointer.
    if unsafe { libc::ioctl(fd, libc::FIONBIO, &mut on) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Rings a bell by its write descriptor. Async-signal-safe. A full pipe
/// is already readable, so a failed write loses nothing.
pub(crate) fn ring(fd: RawFd) {
    let one: u8 = 1;
    // SAFETY: writes one byte from a valid u8 to a non-blocking pipe.
    unsafe { libc::write(fd, (&one as *const u8).cast(), 1) };
}

/// Resets a bell by its read descriptor: reads the pipe empty.
pub(crate) fn drain(fd: RawFd) {
    let mut buf = [0u8; 64];
    // SAFETY: reads into a valid buffer from a non-blocking pipe.
    while unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) } == buf.len() as isize {}
}

/// The calling thread's `errno`, for a signal handler to save and restore.
pub(crate) fn errno_location() -> *mut i32 {
    // SAFETY: always valid for the calling thread.
    unsafe { libc::__error() }
}

/// CPUs the process may use. macOS has no affinity masks, so every CPU.
pub(crate) fn allowed_cpus() -> Vec<usize> {
    let n = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    (0..n).collect()
}

/// macOS cannot pin a thread to a CPU (its affinity API is only a hint to
/// keep threads apart, and Apple Silicon ignores it).
pub(crate) const CAN_PIN: bool = false;

/// Does nothing: see [`CAN_PIN`].
pub(crate) fn pin_to_cpu(_cpu: usize) {}

/// Fills `buf` from the system's random source; never fails.
pub(crate) fn random_bytes(buf: &mut [u8]) -> bool {
    // SAFETY: writes exactly `buf.len()` bytes into `buf`.
    unsafe { libc::arc4random_buf(buf.as_mut_ptr().cast(), buf.len()) };
    true
}

/// Accepts a connection as a close-on-exec, non-blocking descriptor;
/// -1 with `errno` set on failure. macOS has no `accept4`, so the flags are
/// set after the accept: a `fork` and `exec` on another thread in between
/// would see the descriptor (the same window std has on macOS).
pub(crate) fn accept(fd: RawFd) -> isize {
    // SAFETY: null address pointers ask for no peer address.
    let new = unsafe { libc::accept(fd, std::ptr::null_mut(), std::ptr::null_mut()) };
    if new < 0 {
        return -1;
    }
    if set_cloexec(new).is_err() || set_nonblocking(new).is_err() {
        let e = io::Error::last_os_error();
        // SAFETY: the descriptor is ours and is closed once.
        unsafe { libc::close(new) };
        // SAFETY: restores the error of the failed call for the caller.
        unsafe { *libc::__error() = e.raw_os_error().unwrap_or(libc::EIO) };
        return -1;
    }
    new as isize
}

/// `pread(2)`.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes of writes.
pub(crate) unsafe fn pread(fd: RawFd, buf: *mut u8, len: usize, offset: u64) -> isize {
    no_offsets_on_a_terminal(libc::pread(fd, buf.cast(), len, offset as libc::off_t))
}

/// macOS refuses `pread` and `pwrite` on a terminal with `ENXIO`, where a
/// pipe (and everything without offsets on Linux) gets `ESPIPE`: reported
/// as the latter, which the callers answer with a plain read or write.
fn no_offsets_on_a_terminal(res: isize) -> isize {
    // SAFETY: the calling thread's own `errno`.
    unsafe {
        if res < 0 && *libc::__error() == libc::ENXIO {
            *libc::__error() = libc::ESPIPE;
        }
    }
    res
}

/// `pwrite(2)`.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes of reads.
pub(crate) unsafe fn pwrite(fd: RawFd, buf: *const u8, len: usize, offset: u64) -> isize {
    // Linux's pwrite appends on an `O_APPEND` descriptor whatever the
    // offset, which `OpenOptions::append` promises; macOS writes at the
    // offset. So an append-mode file is written with write(2).
    if libc::fcntl(fd, libc::F_GETFL) & libc::O_APPEND != 0 {
        return libc::write(fd, buf.cast(), len);
    }
    no_offsets_on_a_terminal(libc::pwrite(fd, buf.cast(), len, offset as libc::off_t))
}

/// `st_mode` of an open descriptor, from `fstat`.
pub(crate) fn fd_mode(fd: RawFd) -> Option<u32> {
    // SAFETY: all-zero is a valid `stat`, which fstat fills in.
    let mut st: libc::stat = unsafe { mem::zeroed() };
    // SAFETY: a valid pointer to a `stat`.
    (unsafe { libc::fstat(fd, &mut st) } == 0).then_some(u32::from(st.st_mode))
}

/// Flushes the file to stable storage. `fsync` on macOS only hands the data
/// to the drive, whose cache may lose it, so this uses `F_FULLFSYNC`, as
/// std's `sync_all` and `sync_data` do; macOS has no cheaper `fdatasync`.
pub(crate) fn fsync(fd: RawFd, _datasync: bool) -> isize {
    // SAFETY: plain fcntl on a descriptor.
    let res = unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) };
    if res < 0 && full_fsync_unsupported(io::Error::last_os_error().raw_os_error()) {
        // A filesystem without F_FULLFSYNC (some network ones): plain fsync
        // is the most it offers. Any other error (EIO above all: the flush
        // failed) is the answer, as in std, which never falls back.
        // SAFETY: as above.
        return unsafe { libc::fsync(fd) } as isize;
    }
    res as isize
}

/// Whether an `F_FULLFSYNC` failure only says that the filesystem does not
/// have it.
fn full_fsync_unsupported(errno: Option<i32>) -> bool {
    matches!(errno, Some(libc::ENOTSUP | libc::ENOTTY | libc::EINVAL))
}

/// Reserves space up to `offset + len` and extends the file to that size,
/// like `fallocate(2)` with mode 0: `F_PREALLOCATE` (contiguous if it can,
/// else anywhere), then `ftruncate` if the file was shorter.
pub(crate) fn fallocate(fd: RawFd, offset: u64, len: u64) -> isize {
    let Some(end) = offset.checked_add(len).filter(|&e| e <= i64::MAX as u64) else {
        // SAFETY: sets errno for the caller.
        unsafe { *libc::__error() = libc::EFBIG };
        return -1;
    };
    // fallocate(2) refuses a descriptor not open for writing with EBADF;
    // F_PREALLOCATE does not check, and a range inside the file is a no-op.
    // SAFETY: plain fcntl on a descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return -1;
    }
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        // SAFETY: sets errno for the caller.
        unsafe { *libc::__error() = libc::EBADF };
        return -1;
    }
    // SAFETY: all-zero is a valid `stat`, which fstat fills in.
    let mut st: libc::stat = unsafe { mem::zeroed() };
    // SAFETY: a valid pointer to a `stat`.
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return -1;
    }
    let size = st.st_size as u64;
    if end <= size {
        return 0;
    }
    let mut store = libc::fstore_t {
        // F_ALLOCATEALL: all of it or nothing; without it a partial
        // reservation succeeds.
        fst_flags: libc::F_ALLOCATECONTIG | libc::F_ALLOCATEALL,
        fst_posmode: libc::F_PEOFPOSMODE,
        fst_offset: 0,
        fst_length: (end - size) as libc::off_t,
        fst_bytesalloc: 0,
    };
    // SAFETY: F_PREALLOCATE reads and writes a valid `fstore_t`.
    if unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut store) } < 0 {
        store.fst_flags = libc::F_ALLOCATEALL;
        // SAFETY: as above.
        if unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &mut store) } < 0 {
            return -1;
        }
    }
    // SAFETY: plain syscall on a descriptor.
    unsafe { libc::ftruncate(fd, end as libc::off_t) as isize }
}

/// Sends up to `count` bytes of `file` from `*offset` to socket `sock`,
/// advancing `*offset`. Returns the bytes sent, 0 at the end of the file,
/// or -1 with `errno` set. macOS's `sendfile` takes the file first and
/// reports a partial send through its length argument, even when it fails
/// with `EAGAIN`.
pub(crate) fn sendfile(sock: RawFd, file: RawFd, offset: &mut u64, count: usize) -> isize {
    let mut len = count as libc::off_t;
    // SAFETY: plain system call; `len` is a valid `off_t` the kernel reads
    // and overwrites; no headers or trailers.
    let res = unsafe {
        libc::sendfile(
            file,
            sock,
            *offset as libc::off_t,
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if len > 0 {
        *offset += len as u64;
        return len as isize;
    }
    if res == 0 {
        return 0;
    }
    // A peer that closed makes macOS answer "not connected" where a send
    // (and Linux's sendfile) says "broken pipe": reported as the latter, so
    // one error means one thing on every system.
    // SAFETY: the calling thread's own `errno`.
    unsafe {
        if *libc::__error() == libc::ENOTCONN {
            *libc::__error() = libc::EPIPE;
        }
    }
    -1
}

/// Reads a batch of directory entries into `buf` (whose capacity is the
/// batch size) as `linux_dirent64` records, the layout `fs::ReadDir`
/// parses: `d_ino: u64, d_off: i64, d_reclen: u16, d_type: u8, d_name`
/// NUL-terminated, each record padded to 8 bytes. macOS has no public
/// `getdents`, so the records are built from `readdir(3)` on a duplicate of
/// `dir`. Returns the bytes written; 0 at the end.
pub(crate) fn read_dir_batch(
    state: &mut DirState,
    dir: RawFd,
    buf: &mut Vec<u8>,
) -> io::Result<usize> {
    buf.clear();
    let cap = buf.capacity();
    if state.dir.is_null() {
        // SAFETY: plain syscalls; `fdopendir` owns the duplicate on success.
        unsafe {
            let dup = libc::fcntl(dir, libc::F_DUPFD_CLOEXEC, 0);
            if dup < 0 {
                return Err(io::Error::last_os_error());
            }
            let d = libc::fdopendir(dup);
            if d.is_null() {
                let e = io::Error::last_os_error();
                libc::close(dup);
                return Err(e);
            }
            state.dir = d;
        }
    }
    loop {
        let (ino, d_type, name) = match state.pending.take() {
            Some(entry) => entry,
            None => {
                // SAFETY: `state.dir` is an open stream only this state uses;
                // errno is cleared first to tell the end from an error.
                let ent = unsafe {
                    *libc::__error() = 0;
                    libc::readdir(state.dir)
                };
                if ent.is_null() {
                    let errno = io::Error::last_os_error();
                    if errno.raw_os_error().unwrap_or(0) != 0 {
                        return Err(errno);
                    }
                    return Ok(buf.len());
                }
                // SAFETY: readdir returned a valid entry, valid until the
                // next call; the name is copied out at once.
                unsafe {
                    let e = &*ent;
                    let name = std::slice::from_raw_parts(
                        e.d_name.as_ptr().cast::<u8>(),
                        usize::from(e.d_namlen),
                    );
                    (e.d_ino, e.d_type, name.to_vec())
                }
            }
        };
        let reclen = (19 + name.len() + 1).next_multiple_of(8);
        if buf.len() + reclen > cap {
            if buf.is_empty() {
                // One name longer than the batch: cannot happen (names are
                // at most 1023 bytes), but never loop.
                return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
            }
            state.pending = Some((ino, d_type, name));
            return Ok(buf.len());
        }
        buf.extend_from_slice(&ino.to_ne_bytes());
        buf.extend_from_slice(&0i64.to_ne_bytes());
        buf.extend_from_slice(&(reclen as u16).to_ne_bytes());
        buf.push(d_type);
        buf.extend_from_slice(&name);
        buf.resize(buf.len() + reclen - 19 - name.len(), 0);
    }
}

/// The directory stream [`read_dir_batch`] reads from, and an entry that
/// did not fit in the last batch.
pub(crate) struct DirState {
    dir: *mut libc::DIR,
    pending: Option<(u64, u8, Vec<u8>)>,
}

impl Default for DirState {
    fn default() -> Self {
        Self {
            dir: std::ptr::null_mut(),
            pending: None,
        }
    }
}

// SAFETY: the stream is owned by this value alone and only used through
// `&mut`, so moving it to another thread is fine.
unsafe impl Send for DirState {}
// SAFETY: no method takes `&self`.
unsafe impl Sync for DirState {}

impl Drop for DirState {
    fn drop(&mut self) {
        if !self.dir.is_null() {
            // SAFETY: an open stream we own, closed once.
            unsafe { libc::closedir(self.dir) };
        }
    }
}

/// macOS has no flag that makes a receive return the datagram's real
/// length; worse, flags passed in come back in `msg_flags`, so `MSG_TRUNC`
/// passed in would mark every datagram as cut off.
pub(crate) const RECV_REAL_LEN: libc::c_int = 0;
/// `recvfrom` cannot tell that a datagram was cut off; `recvmsg` sets
/// `MSG_TRUNC` in `msg_flags`.
pub(crate) const RECVFROM_REPORTS_TRUNCATION: bool = false;

/// One entry of [`recvmmsg`], laid out as Linux's `mmsghdr`.
#[repr(C)]
pub(crate) struct MMsgHdr {
    pub(crate) msg_hdr: libc::msghdr,
    pub(crate) msg_len: libc::c_uint,
}

std::thread_local! {
    /// The error the last [`recvmmsg`] of this thread met after the
    /// datagrams it returned, if any (see [`take_recv_error`]).
    static RECV_ERROR: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    /// A descriptor whose [`sendmmsg`] met an error after the datagrams it
    /// sent, and that error, for its next call.
    static SEND_ERROR: std::cell::Cell<(RawFd, i32)> = const { std::cell::Cell::new((-1, 0)) };
}

/// Whether `errno` only says that there was nothing (more) to do.
fn would_block(errno: i32) -> bool {
    errno == libc::EAGAIN || errno == libc::EWOULDBLOCK
}

/// Receives up to `count` datagrams, one `recvmsg` each (macOS has no
/// public `recvmmsg`): how many, or -1 with `errno` set if the first failed.
///
/// A `recvmsg` that fails after the first datagram has taken the error off
/// the socket (a pending `ICMP` port unreachable, say), so nothing would
/// report it again, and a receive waiting for it would wait forever. It is
/// kept for [`take_recv_error`] instead, as Linux keeps it on the socket.
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
    for i in 0..count {
        let m = &mut *msgs.add(i);
        let n = libc::recvmsg(fd, &mut m.msg_hdr, flags);
        if n < 0 {
            if i == 0 {
                return -1;
            }
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if !would_block(errno) {
                RECV_ERROR.with(|e| e.set(errno));
            }
            return i as isize;
        }
        m.msg_len = n as libc::c_uint;
    }
    count as isize
}

/// The error (an `errno`) that the last [`recvmmsg`] of this thread met
/// after the datagrams it returned, for the caller to report on the next
/// receive; taken once.
pub(crate) fn take_recv_error() -> Option<i32> {
    match RECV_ERROR.with(|e| e.replace(0)) {
        0 => None,
        errno => Some(errno),
    }
}

/// macOS cannot read only what the page cache holds: always -1, so every
/// read goes through the driver.
///
/// # Safety
///
/// None needed; the signature matches Linux's.
pub(crate) unsafe fn pread_cached(_fd: RawFd, _iov: &libc::iovec, _offset: u64) -> isize {
    -1
}

/// The `poll` event for "the peer closed its end" (macOS has no
/// `POLLRDHUP`).
pub(crate) const POLL_PEER_CLOSED: libc::c_short = libc::POLLHUP;

/// Sends up to `count` datagrams, one `sendmsg` each (macOS has no public
/// `sendmmsg`): how many, or -1 with `errno` set if the first failed.
///
/// An error after the first datagram may have been taken off the socket
/// (as in [`recvmmsg`]), so it is kept and returned by the next call on
/// `fd`, which the caller makes at once for the datagrams that are left:
/// the datagram that failed gets the error.
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
    let (held, errno) = SEND_ERROR.with(|e| e.get());
    if held == fd && errno != 0 {
        SEND_ERROR.with(|e| e.set((-1, 0)));
        *libc::__error() = errno;
        return -1;
    }
    for i in 0..count {
        let m = &mut *msgs.add(i);
        let n = libc::sendmsg(fd, &m.msg_hdr, flags);
        if n < 0 {
            if i == 0 {
                return -1;
            }
            let errno = io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if !would_block(errno) {
                SEND_ERROR.with(|e| e.set((fd, errno)));
            }
            return i as isize;
        }
        m.msg_len = n as libc::c_uint;
    }
    count as isize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// `fsync` falls back from `F_FULLFSYNC` only where the filesystem does
    /// not have it; a failed flush (`EIO`) is reported, as in std.
    #[test]
    fn only_a_missing_full_fsync_falls_back() {
        for errno in [libc::ENOTSUP, libc::ENOTTY, libc::EINVAL] {
            assert!(full_fsync_unsupported(Some(errno)), "{errno}");
        }
        for errno in [libc::EIO, libc::ENOSPC, libc::EBADF] {
            assert!(!full_fsync_unsupported(Some(errno)), "{errno}");
        }
        assert!(!full_fsync_unsupported(None));
    }

    /// A datagram, then an `ICMP` port unreachable: the batch returns the
    /// datagram and keeps the error, which `recvmsg` took off the socket,
    /// for the next receive (as Linux keeps it on the socket).
    #[test]
    fn a_receive_batch_keeps_the_error_it_met_after_a_datagram() {
        let a = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        a.connect(peer.local_addr().unwrap()).unwrap();
        peer.send_to(b"x", a.local_addr().unwrap()).unwrap();
        drop(peer);
        a.send(b"y").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(100));

        let mut bufs = [[0u8; 8]; 2];
        // SAFETY: all-zero is valid for these plain C structs.
        let mut iov: [libc::iovec; 2] = unsafe { mem::zeroed() };
        // SAFETY: as above.
        let mut msgs: [MMsgHdr; 2] = unsafe { mem::zeroed() };
        for i in 0..2 {
            iov[i].iov_base = bufs[i].as_mut_ptr().cast();
            iov[i].iov_len = 8;
            msgs[i].msg_hdr.msg_iov = &mut iov[i];
            msgs[i].msg_hdr.msg_iovlen = 1;
        }
        // SAFETY: two headers pointing at the buffers above.
        let n = unsafe { recvmmsg(a.as_raw_fd(), msgs.as_mut_ptr(), 2, libc::MSG_DONTWAIT) };
        assert_eq!(n, 1);
        assert_eq!(&bufs[0][..1], b"x");
        assert_eq!(take_recv_error(), Some(libc::ECONNREFUSED));
        assert_eq!(take_recv_error(), None);
    }
}
