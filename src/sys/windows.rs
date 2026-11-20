//! Windows: what the runtime asks of the system outside its driver.
//!
//! Sockets and files are different things here (`SOCKET` and `HANDLE`),
//! addresses and message headers are Winsock's, and paths are UTF-16. The
//! wake-up of a worker is not in this file: on Windows it is a packet
//! posted to the worker's completion port, which the driver owns
//! (`driver::iocp`).

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    self, SOCKADDR, SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, WSABUF, WSAMSG,
};
use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, GetProcessAffinityMask, SetThreadAffinityMask,
};

/// A socket, as operations name it.
pub(crate) type RawSock = SOCKET;

/// A file, pipe or device, as operations name it.
pub(crate) type RawFile = HANDLE;

/// A socket address as Winsock reads it.
pub(crate) type SockAddr = SOCKADDR;

/// The length of a socket address.
pub(crate) type SockLen = i32;

/// Room for any socket address.
pub(crate) type SockAddrStorage = SOCKADDR_STORAGE;

/// The header of a `WSASendMsg` or `WSARecvMsg`.
pub(crate) type MsgHdr = WSAMSG;

/// One unit of a NUL-terminated UTF-16 path.
pub(crate) type PathChar = u16;

/// Windows sockets raise no signal when the peer is gone, so sends need no
/// flag against one.
pub(crate) const MSG_NOSIGNAL: i32 = 0;

/// The negated code of the calling thread's last failed call, the form in
/// which drivers report results. `std::io::Error::from_raw_os_error` reads
/// the positive code, for Win32 and Winsock errors alike.
pub(crate) fn neg_errno() -> i32 {
    // SAFETY: reads the calling thread's last error code.
    -(unsafe { GetLastError() } as i32)
}

/// CPUs the process may run on, within the thread's processor group.
///
/// A group holds at most 64 processors. A machine with more has several
/// groups and a thread runs in one of them; workers on such a machine stay
/// within the group the process started in until pinning learns groups
/// (`SetThreadGroupAffinity`).
pub(crate) fn allowed_cpus() -> Vec<usize> {
    let mut process = 0usize;
    let mut system = 0usize;
    // SAFETY: the pseudo-handle of the current process is always valid;
    // both masks are locals.
    let ok = unsafe { GetProcessAffinityMask(GetCurrentProcess(), &mut process, &mut system) };
    if ok == 0 || process == 0 {
        let n = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        return (0..n.min(usize::BITS as usize)).collect();
    }
    (0..usize::BITS as usize)
        .filter(|&cpu| process & (1 << cpu) != 0)
        .collect()
}

/// `true` where workers can be pinned to a CPU.
pub(crate) const CAN_PIN: bool = true;

/// Pins the calling thread to `cpu`, a CPU of its processor group. Failure
/// is not fatal: the worker just stays unpinned.
pub(crate) fn pin_to_cpu(cpu: usize) {
    if cpu >= usize::BITS as usize {
        return;
    }
    // SAFETY: the pseudo-handle of the current thread is always valid.
    unsafe { SetThreadAffinityMask(GetCurrentThread(), 1 << cpu) };
}

/// Fills `buf` from the system's random source; `false` if it failed.
pub(crate) fn random_bytes(buf: &mut [u8]) -> bool {
    // The process-wide generator Windows seeds from the kernel; it is the
    // source the standard library uses, and it cannot fail on Windows 10
    // and later.
    #[link(name = "bcryptprimitives", kind = "raw-dylib")]
    extern "system" {
        fn ProcessPrng(data: *mut u8, len: usize) -> i32;
    }
    // SAFETY: writes exactly `buf.len()` bytes into `buf`.
    unsafe { ProcessPrng(buf.as_mut_ptr(), buf.len()) != 0 }
}

/// Flushes a file's data and metadata to the device. Windows has no call
/// that leaves the metadata out, so `datasync` changes nothing.
pub(crate) fn fsync(file: RawFile, _datasync: bool) -> isize {
    // SAFETY: flushes a handle the caller owns.
    if unsafe { FlushFileBuffers(file) } != 0 {
        0
    } else {
        neg_errno() as isize
    }
}

/// Error numbers the code outside the driver compares results against:
/// the Winsock and Win32 codes that mean what the Unix names say.
pub(crate) mod errno {
    use windows_sys::Win32::Foundation::ERROR_IO_DEVICE;
    use windows_sys::Win32::Networking::WinSock::{
        WSAEADDRINUSE, WSAEINTR, WSAEOPNOTSUPP, WSAEWOULDBLOCK,
    };

    pub(crate) const EADDRINUSE: i32 = WSAEADDRINUSE;
    pub(crate) const EAGAIN: i32 = WSAEWOULDBLOCK;
    pub(crate) const EINTR: i32 = WSAEINTR;
    pub(crate) const EIO: i32 = ERROR_IO_DEVICE as i32;
    pub(crate) const EOPNOTSUPP: i32 = WSAEOPNOTSUPP;
}

/// Reserves and commits `len` bytes of zero-filled, page-aligned memory for
/// a buffer pool. Windows gives a committed page physical memory when it is
/// first touched, so an idle pool costs commit charge, not memory. Null if
/// the system refuses; `io::Error::last_os_error` then says why.
pub(crate) fn map_pool(len: usize) -> *mut u8 {
    // SAFETY: a fresh allocation at an address the system picks.
    unsafe {
        VirtualAlloc(
            std::ptr::null(),
            len,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        )
        .cast()
    }
}

/// Gives back memory from [`map_pool`].
///
/// # Safety
///
/// `ptr` is the start of a region `map_pool` returned, and nothing uses it
/// any more.
pub(crate) unsafe fn unmap_pool(ptr: *mut u8, _len: usize) {
    // SAFETY: the caller hands a region allocated by `map_pool`; releasing
    // takes the base address and a size of 0.
    unsafe { VirtualFree(ptr.cast(), 0, MEM_RELEASE) };
}

/// The size of a page of memory.
pub(crate) fn page_size() -> usize {
    // SAFETY: fills a local with plain data.
    let info = unsafe {
        let mut info: SYSTEM_INFO = std::mem::zeroed();
        GetSystemInfo(&mut info);
        info
    };
    info.dwPageSize as usize
}

/// `true` if a send with `flags` waits for room in the socket's buffer:
/// always here, where no flag asks an overlapped send not to.
#[inline(always)]
pub(crate) fn send_waits(_flags: i32) -> bool {
    true
}

/// One piece of a vectored send or receive.
pub(crate) type IoVec = WSABUF;

/// An IPv6 socket address, the largest giong reads from a datagram.
pub(crate) type SockAddrIn6 = SOCKADDR_IN6;

/// The address family of IPv4, as a socket address carries it.
pub(crate) const AF_INET: i32 = WinSock::AF_INET as i32;
/// The address family of IPv6.
pub(crate) const AF_INET6: i32 = WinSock::AF_INET6 as i32;
/// The flag a received message carries when its end was cut off.
pub(crate) const MSG_TRUNC: i32 = WinSock::MSG_TRUNC as i32;

/// The piece of `len` bytes at `base`. A Winsock piece holds less than
/// 4 GiB: a longer one is cut there, and the send or receive reports the
/// shorter count like any partial transfer.
#[inline(always)]
pub(crate) fn io_vec(base: *const u8, len: usize) -> IoVec {
    WSABUF {
        len: len.min(u32::MAX as usize) as u32,
        buf: base.cast_mut(),
    }
}

/// Where a piece starts and how long it is.
#[inline(always)]
pub(crate) fn io_vec_parts(v: &IoVec) -> (*const u8, usize) {
    (v.buf.cast_const(), v.len as usize)
}

/// Points a piece at `base`, keeping its length.
#[inline(always)]
pub(crate) fn io_vec_set_base(v: &mut IoVec, base: *const u8) {
    v.buf = base.cast_mut();
}

/// The header of a receive that only says how much room the sender's
/// address gets: `name_len` bytes, and nothing else.
pub(crate) const fn recv_header(name_len: usize) -> MsgHdr {
    // SAFETY: all-zero is a valid `WSAMSG` (null pointers, zero lengths).
    let mut hdr: MsgHdr = unsafe { std::mem::zeroed() };
    hdr.namelen = name_len as SockLen;
    hdr
}

/// Points a message header at `count` pieces.
///
/// # Safety
///
/// `msg` must point at a live header.
#[inline(always)]
pub(crate) unsafe fn msg_set_iov(msg: *mut MsgHdr, iov: *mut IoVec, count: usize) {
    (*msg).lpBuffers = iov;
    (*msg).dwBufferCount = count as u32;
}

/// Points a message header at `len` bytes of ancillary data.
///
/// # Safety
///
/// `msg` must point at a live header.
#[inline(always)]
pub(crate) unsafe fn msg_set_control(msg: *mut MsgHdr, control: *mut u8, len: usize) {
    (*msg).Control = io_vec(control, len);
}

/// Points a message header at an address of `len` bytes, or at room for one.
///
/// # Safety
///
/// `msg` must point at a live header.
#[inline(always)]
pub(crate) unsafe fn msg_set_name(msg: *mut MsgHdr, name: *mut u8, len: SockLen) {
    (*msg).name = name.cast();
    (*msg).namelen = len;
}

/// The length of the address in a message header.
#[inline(always)]
pub(crate) fn msg_name_len(msg: &MsgHdr) -> usize {
    msg.namelen as usize
}

/// `send` on a non-blocking socket: the bytes sent, or -1 with the thread's
/// last error set (`WSAEWOULDBLOCK` when the socket has no room). Every
/// socket a worker's port knows is non-blocking.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes.
#[inline(always)]
pub(crate) unsafe fn send_nowait(sock: RawSock, buf: *const u8, len: usize) -> isize {
    WinSock::send(sock, buf, len.min(i32::MAX as usize) as i32, 0) as isize
}

/// `sendto` on a non-blocking socket: the bytes sent, or -1 with the
/// thread's last error set.
///
/// # Safety
///
/// `buf` must be valid for `len` bytes and `addr` for `addr_len`.
#[inline(always)]
pub(crate) unsafe fn send_to_nowait(
    sock: RawSock,
    buf: *const u8,
    len: usize,
    addr: *const SockAddr,
    addr_len: SockLen,
) -> isize {
    WinSock::sendto(
        sock,
        buf,
        len.min(i32::MAX as usize) as i32,
        0,
        addr,
        addr_len,
    ) as isize
}

/// The names `net` uses for a socket's raw handle. On Unix they are the
/// standard library's descriptor traits; here the same names reach the
/// socket handle, so that the code between the two reads the same.
pub(crate) mod fd {
    use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket};

    /// A socket handle.
    pub(crate) type RawFd = super::RawSock;

    pub(crate) trait AsRawFd {
        fn as_raw_fd(&self) -> RawFd;
    }

    impl<T: AsRawSocket> AsRawFd for T {
        fn as_raw_fd(&self) -> RawFd {
            self.as_raw_socket() as RawFd
        }
    }

    pub(crate) trait FromRawFd {
        /// # Safety
        ///
        /// `raw` must be an open socket nothing else owns.
        unsafe fn from_raw_fd(raw: RawFd) -> Self;
    }

    impl<T: FromRawSocket> FromRawFd for T {
        unsafe fn from_raw_fd(raw: RawFd) -> Self {
            Self::from_raw_socket(raw as _)
        }
    }

    pub(crate) trait IntoRawFd {
        fn into_raw_fd(self) -> RawFd;
    }

    impl<T: IntoRawSocket> IntoRawFd for T {
        fn into_raw_fd(self) -> RawFd {
            self.into_raw_socket() as RawFd
        }
    }
}

/// What runs when the console asks the process to stop.
type OnSignal = std::sync::Arc<dyn Fn() + Send + Sync>;

/// The runtimes that asked to hear console events, each under its own
/// number so that it can leave again.
static ON_SIGNAL: std::sync::Mutex<Vec<(u64, OnSignal)>> = std::sync::Mutex::new(Vec::new());

/// Called by Windows, on a thread it starts for the purpose, for Ctrl+C and
/// Ctrl+Break. `TRUE` says the event is handled, so the process is not
/// ended; with no runtime listening the default handler ends it as usual.
unsafe extern "system" fn console_event(kind: u32) -> BOOL {
    if kind != CTRL_C_EVENT && kind != CTRL_BREAK_EVENT {
        return 0;
    }
    let listeners: Vec<OnSignal> = match ON_SIGNAL.lock() {
        Ok(list) => list.iter().map(|(_, f)| f.clone()).collect(),
        Err(_) => return 0,
    };
    for on_signal in &listeners {
        on_signal();
    }
    BOOL::from(!listeners.is_empty())
}

/// While it lives, Ctrl+C and Ctrl+Break in the process's console run the
/// function it was installed with instead of ending the process: what
/// `SIGINT` and `SIGTERM` are on Unix.
pub(crate) struct SignalGuard {
    id: u64,
}

impl SignalGuard {
    /// Starts listening. `on_signal` runs on a thread Windows starts for
    /// each event, so it may block and take locks.
    pub(crate) fn install(on_signal: impl Fn() + Send + Sync + 'static) -> std::io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        static HANDLER: std::sync::Once = std::sync::Once::new();
        let mut installed = Ok(());
        HANDLER.call_once(|| {
            // SAFETY: `console_event` has the signature of a handler routine
            // and stays valid for the life of the process.
            if unsafe { SetConsoleCtrlHandler(Some(console_event), 1) } == 0 {
                installed = Err(std::io::Error::last_os_error());
            }
        });
        installed?;
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        ON_SIGNAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((id, std::sync::Arc::new(on_signal)));
        Ok(SignalGuard { id })
    }
}

impl Drop for SignalGuard {
    fn drop(&mut self) {
        ON_SIGNAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(id, _)| *id != self.id);
    }
}

/// What [`AcceptEvents::wait`] woke up for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AcceptWake {
    /// Connections may be waiting on the listener.
    Accept,
    /// [`AcceptEvents::stop`] was called.
    Stop,
    /// Neither, within the time given.
    Timeout,
}

/// Lets a thread that is not a worker wait for connections on a listening
/// socket, and be told to stop.
///
/// The wait is a `select` on the listener and on a datagram socket that is
/// connected to itself: [`stop`](Self::stop) sends that socket a byte,
/// which nobody reads, so it stays readable. `select` reports what is true
/// at the moment it is asked, so a connection that arrives at any point is
/// seen by the next wait (an event object set by `WSAEventSelect` can be
/// reset past one).
pub(crate) struct AcceptEvents {
    listener: RawSock,
    wake: RawSock,
    /// Set by `stop`, and looked at every so often in case the byte could
    /// not be sent.
    stopped: std::sync::atomic::AtomicBool,
}

impl AcceptEvents {
    /// Prepares to wait on `listener`, which the caller keeps open for as
    /// long as this lives, and makes it non-blocking.
    pub(crate) fn new(listener: RawSock) -> std::io::Result<Self> {
        // SAFETY: plain Winsock calls on a new socket, which is closed on
        // every failure, and on the caller's listener; the address is a
        // local of the size given.
        unsafe {
            let mut on = 1u32;
            if WinSock::ioctlsocket(listener, WinSock::FIONBIO, &mut on) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            let wake = WinSock::socket(
                WinSock::AF_INET as i32,
                WinSock::SOCK_DGRAM,
                WinSock::IPPROTO_UDP,
            );
            if wake == WinSock::INVALID_SOCKET {
                return Err(std::io::Error::last_os_error());
            }
            let events = AcceptEvents {
                listener,
                wake,
                stopped: std::sync::atomic::AtomicBool::new(false),
            };
            let mut addr: WinSock::SOCKADDR_IN = std::mem::zeroed();
            addr.sin_family = WinSock::AF_INET;
            addr.sin_addr.S_un.S_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            let mut len = std::mem::size_of::<WinSock::SOCKADDR_IN>() as i32;
            let name = (&mut addr as *mut WinSock::SOCKADDR_IN).cast::<SOCKADDR>();
            if WinSock::bind(wake, name, len) != 0
                || WinSock::getsockname(wake, name, &mut len) != 0
                || WinSock::connect(wake, name, len) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(events)
        }
    }

    /// Waits for a connection or for [`stop`](Self::stop), at most
    /// `timeout` milliseconds (`None`: without limit).
    pub(crate) fn wait(&self, timeout: Option<u32>) -> AcceptWake {
        loop {
            if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
                return AcceptWake::Stop;
            }
            match self.wait_once(timeout.unwrap_or(100)) {
                AcceptWake::Timeout if timeout.is_none() => {}
                woke => return woke,
            }
        }
    }

    fn wait_once(&self, timeout: u32) -> AcceptWake {
        // SAFETY: the set and the timeout are locals; both sockets are
        // open while `self` lives.
        unsafe {
            let mut read: WinSock::FD_SET = std::mem::zeroed();
            read.fd_count = 2;
            read.fd_array[0] = self.listener;
            read.fd_array[1] = self.wake;
            let limit = WinSock::TIMEVAL {
                tv_sec: (timeout / 1000) as i32,
                tv_usec: (timeout % 1000 * 1000) as i32,
            };
            let ready = WinSock::select(
                0,
                &mut read,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &limit,
            );
            let set = &read.fd_array[..(read.fd_count as usize).min(2)];
            if ready > 0 && set.contains(&self.wake) {
                AcceptWake::Stop
            } else if ready > 0 {
                AcceptWake::Accept
            } else {
                AcceptWake::Timeout
            }
        }
    }

    /// Makes [`wait`](Self::wait) return [`AcceptWake::Stop`], now and
    /// from here on.
    pub(crate) fn stop(&self) {
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        // SAFETY: one byte from a local to the socket's own address.
        unsafe { WinSock::send(self.wake, [1u8].as_ptr(), 1, 0) };
    }
}

impl Drop for AcceptEvents {
    fn drop(&mut self) {
        // SAFETY: the socket is owned by `self` and closed once.
        unsafe { WinSock::closesocket(self.wake) };
    }
}

/// Takes one waiting connection from the non-blocking `listener`: the
/// socket, `None` if none waits, or the error.
pub(crate) fn accept_nowait(listener: RawSock) -> std::io::Result<Option<RawSock>> {
    // SAFETY: `accept` without an address; the socket it returns is new
    // and owned by the caller.
    unsafe {
        let sock = WinSock::accept(listener, std::ptr::null_mut(), std::ptr::null_mut());
        if sock == WinSock::INVALID_SOCKET {
            let err = std::io::Error::last_os_error();
            return match err.raw_os_error() {
                Some(WinSock::WSAEWOULDBLOCK) => Ok(None),
                _ => Err(err),
            };
        }
        Ok(Some(sock))
    }
}
