//! Windows: what the runtime asks of the system outside its driver.
//!
//! Sockets and files are different things here (`SOCKET` and `HANDLE`),
//! addresses and message headers are Winsock's, and paths are UTF-16. The
//! wake-up of a worker is not in this file: on Windows it is a packet
//! posted to the worker's completion port, which the driver owns
//! (`driver::iocp`).

use std::{io, mem};

use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{
    self, SOCKADDR, SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, WSABUF, WSAMSG,
};
use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, ReadFile, WriteFile};
use windows_sys::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};
use windows_sys::Win32::System::Memory::{
    VirtualAlloc, VirtualFree, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, GetProcessAffinityMask,
    SetThreadAffinityMask,
};
use windows_sys::Win32::System::IO::{GetOverlappedResult, OVERLAPPED};

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
    // 32-bit x86 decorates `stdcall` names (`_ProcessPrng@8`); the DLL
    // exports the plain name, as the standard library declares it.
    #[cfg_attr(
        target_arch = "x86",
        link(
            name = "bcryptprimitives",
            kind = "raw-dylib",
            import_name_type = "undecorated"
        )
    )]
    #[cfg_attr(
        not(target_arch = "x86"),
        link(name = "bcryptprimitives", kind = "raw-dylib")
    )]
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

/// Whether `handle` is a character device, a console above all: those
/// take no overlapped calls and cannot join a completion port, so their
/// operations run on the blocking threads.
pub(crate) fn is_char_device(handle: std::os::windows::io::RawHandle) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{GetFileType, FILE_TYPE_CHAR};
    // SAFETY: only reads the type of a handle the caller owns.
    unsafe { GetFileType(handle) == FILE_TYPE_CHAR }
}

/// `CreateFileW` flag: the file is opened for overlapped I/O.
pub(crate) const FILE_FLAG_OVERLAPPED: u32 =
    windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

/// An event of the calling thread for the reads and writes below to wait
/// on, made at the thread's first use and closed when the thread ends (the
/// blocking pool's threads end after a while without work).
fn thread_event() -> io::Result<HANDLE> {
    EVENT.with(|event| {
        if event.0.get().is_null() {
            // SAFETY: creates an unnamed manual-reset event, not signalled.
            let made = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if made.is_null() {
                return Err(io::Error::last_os_error());
            }
            event.0.set(made);
        }
        Ok(event.0.get())
    })
}

thread_local! {
    static EVENT: ThreadEvent = const { ThreadEvent(std::cell::Cell::new(std::ptr::null_mut())) };
}

/// The event of [`thread_event`], closed with its thread.
struct ThreadEvent(std::cell::Cell<HANDLE>);

impl Drop for ThreadEvent {
    fn drop(&mut self) {
        let event = self.0.get();
        if !event.is_null() {
            // SAFETY: the event this thread made, which nothing waits on any
            // more: only this thread's transfers used it.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(event) };
            #[cfg(test)]
            closed_events().lock().unwrap().push(event as usize);
        }
    }
}

/// The events closed by ending threads, for the test below.
#[cfg(test)]
fn closed_events() -> &'static std::sync::Mutex<Vec<usize>> {
    static CLOSED: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
    &CLOSED
}

/// Runs a `ReadFile` or `WriteFile` at `offset` to its end on the calling
/// (blocking) thread, whether or not the handle is open for overlapped I/O.
///
/// # Safety
///
/// `start` must be sound to call with the OVERLAPPED it is given.
unsafe fn transfer_at(
    file: RawFile,
    offset: u64,
    start: impl FnOnce(*mut u32, *mut OVERLAPPED) -> i32,
) -> io::Result<usize> {
    use windows_sys::Win32::Foundation::{
        ERROR_BROKEN_PIPE, ERROR_HANDLE_EOF, ERROR_IO_PENDING, ERROR_MORE_DATA,
    };
    // SAFETY: OVERLAPPED is plain data; all-zero is its initial state.
    let mut overlapped: OVERLAPPED = unsafe { mem::zeroed() };
    overlapped.Anonymous.Anonymous.Offset = offset as u32;
    overlapped.Anonymous.Anonymous.OffsetHigh = (offset >> 32) as u32;
    // An overlapped handle signals this event at the end, and the low bit
    // keeps the completion away from a port the handle may belong to.
    overlapped.hEvent = (thread_event()? as usize | 1) as HANDLE;
    let mut done = 0u32;
    let mut ok = start(&mut done, &mut overlapped);
    // SAFETY: reads the calling thread's last error code.
    if ok == 0 && unsafe { GetLastError() } == ERROR_IO_PENDING {
        // SAFETY: waits for the operation started above on this handle;
        // `overlapped` lives until it returns.
        ok = unsafe { GetOverlappedResult(file, &overlapped, &mut done, 1) };
    }
    if ok != 0 {
        return Ok(done as usize);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error().map(|code| code as u32) {
        // The end of a file, or of a pipe whose writer is gone.
        Some(ERROR_HANDLE_EOF | ERROR_BROKEN_PIPE) => Ok(0),
        // Part of a pipe message: what came, as a stream read.
        // The count is in the OVERLAPPED where the call left `done` unset.
        Some(ERROR_MORE_DATA) => Ok((done as usize).max(overlapped.InternalHigh)),
        _ => Err(err),
    }
}

/// Reads up to `len` bytes at `offset` into `buf`, waiting on the calling
/// thread; 0 at the end of the file.
///
/// # Safety
///
/// `buf` must be valid for writes of `len` bytes.
pub(crate) unsafe fn read_at(
    file: RawFile,
    buf: *mut u8,
    len: u32,
    offset: u64,
) -> io::Result<usize> {
    // SAFETY: the caller guarantees `buf`; the rest is local to the call.
    unsafe {
        transfer_at(file, offset, |done, overlapped| {
            ReadFile(file, buf, len, done, overlapped)
        })
    }
}

/// Writes `len` bytes from `buf` at `offset`, waiting on the calling
/// thread.
///
/// # Safety
///
/// `buf` must be valid for reads of `len` bytes.
pub(crate) unsafe fn write_at(
    file: RawFile,
    buf: *const u8,
    len: u32,
    offset: u64,
) -> io::Result<usize> {
    // SAFETY: the caller guarantees `buf`; the rest is local to the call.
    unsafe {
        transfer_at(file, offset, |done, overlapped| {
            WriteFile(file, buf, len, done, overlapped)
        })
    }
}

/// Error numbers the code outside the driver compares results against:
/// the Winsock and Win32 codes that mean what the Unix names say.
pub(crate) mod errno {
    use windows_sys::Win32::Foundation::ERROR_IO_DEVICE;
    use windows_sys::Win32::Networking::WinSock::{
        WSAEADDRINUSE, WSAECONNRESET, WSAEINTR, WSAEMSGSIZE, WSAENETRESET, WSAEOPNOTSUPP,
        WSAEWOULDBLOCK,
    };

    pub(crate) const EADDRINUSE: i32 = WSAEADDRINUSE;
    pub(crate) const EAGAIN: i32 = WSAEWOULDBLOCK;
    pub(crate) const EINTR: i32 = WSAEINTR;
    pub(crate) const EIO: i32 = ERROR_IO_DEVICE as i32;
    pub(crate) const EMSGSIZE: i32 = WSAEMSGSIZE;
    pub(crate) const ENETRESET: i32 = WSAENETRESET;
    /// A datagram sent earlier was refused by its peer.
    pub(crate) const NO_PEER: i32 = WSAECONNRESET;
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

/// The piece of `len` bytes at `base`. Windows reports counts as signed
/// 32-bit numbers, so a piece holds at most `i32::MAX` bytes (2 GiB): a
/// longer one is cut there, and the send or receive reports the
/// shorter count like any partial transfer.
#[inline(always)]
pub(crate) fn io_vec(base: *const u8, len: usize) -> IoVec {
    WSABUF {
        len: len.min(i32::MAX as usize) as u32,
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

/// Console events seen since a runtime started listening with none
/// listening before it.
static SIGNALS_SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// When the first of [`SIGNALS_SEEN`] came.
static FIRST_SIGNAL_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Events this soon after the first are the same press: Ctrl+C reaches
/// every process of the console, and a wrapper that also passes one on
/// (a file watcher, a dev server) makes a second arrive within
/// milliseconds. Such an event neither asks again nor ends the process.
const SAME_PRESS: std::time::Duration = std::time::Duration::from_millis(250);

/// Called by Windows, on a thread it starts for the purpose, for Ctrl+C and
/// Ctrl+Break, and when the console is closed or the user logs off or the
/// system shuts down. `TRUE` says the event is handled, so the process is
/// not ended; with no runtime listening the default handler ends it as
/// usual.
///
/// The last three are what `SIGTERM` is on Unix, with a difference: the
/// process ends as soon as the handler returns, or a few seconds after the
/// event at the latest. So for those the handler waits, which gives the
/// runtimes that time to shut down; the process ends when its main thread
/// does, or when Windows ends it.
///
/// Only the first event asks for a shutdown. A second one, while the
/// shutdown has not finished, is left to the default handler, which ends
/// the process, as a second `SIGINT` or `SIGTERM` does on Unix: a shutdown
/// that hangs can still be stopped with a second Ctrl+C. An event less
/// than [`SAME_PRESS`] after the first counts as the first again.
unsafe extern "system" fn console_event(kind: u32) -> BOOL {
    let ends_process = matches!(
        kind,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    );
    if kind != CTRL_C_EVENT && kind != CTRL_BREAK_EVENT && !ends_process {
        return 0;
    }
    let now = std::time::Instant::now();
    let mut first_at = FIRST_SIGNAL_AT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let first = SIGNALS_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0;
    let again = match *first_at {
        _ if first => {
            *first_at = Some(now);
            false
        }
        Some(at) if now.duration_since(at) < SAME_PRESS => true,
        _ => return 0,
    };
    drop(first_at);
    let listeners: Vec<OnSignal> = match ON_SIGNAL.lock() {
        Ok(list) => list.iter().map(|(_, f)| f.clone()).collect(),
        Err(_) => return 0,
    };
    if !again {
        for on_signal in &listeners {
            on_signal();
        }
    }
    if ends_process && !listeners.is_empty() {
        // Longer than Windows waits (5 s for a closed console).
        std::thread::sleep(std::time::Duration::from_secs(30));
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
        // The outcome of the one install is kept: a failure is reported to
        // every caller, not only the first.
        static HANDLER: std::sync::OnceLock<Option<i32>> = std::sync::OnceLock::new();
        let failed = *HANDLER.get_or_init(|| {
            // SAFETY: `console_event` has the signature of a handler routine
            // and stays valid for the life of the process.
            if unsafe { SetConsoleCtrlHandler(Some(console_event), 1) } == 0 {
                std::io::Error::last_os_error().raw_os_error()
            } else {
                None
            }
        });
        if let Some(code) = failed {
            return Err(std::io::Error::from_raw_os_error(code));
        }
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut list = ON_SIGNAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Forget events from before any runtime listened. Not while another
        // runtime listens: one whose shutdown hangs must still be stopped
        // by the next Ctrl+C.
        if list.is_empty() {
            SIGNALS_SEEN.store(0, std::sync::atomic::Ordering::SeqCst);
        }
        list.push((id, std::sync::Arc::new(on_signal)));
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

/// Binds of `sock` are exclusive (`SO_EXCLUSIVEADDRUSE`): no other socket
/// can bind its port, `SO_REUSEADDR` or not, while it is open. Set before
/// the bind.
pub(crate) fn set_exclusive_address(sock: &socket2::Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{setsockopt, SOL_SOCKET, SO_EXCLUSIVEADDRUSE};
    let on: BOOL = 1;
    // SAFETY: an open socket; the value is a local of the size given.
    let rc = unsafe {
        setsockopt(
            sock.as_raw_socket() as RawSock,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&on as *const BOOL).cast(),
            mem::size_of::<BOOL>() as i32,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Readies a UDP socket for the runtime, once, when it is made or wrapped:
///
/// - Non-blocking, so the sends that do not wait (`send_to_nowait`) never
///   block the worker. The driver sets it too, but only at the socket's
///   first operation, and a send can come before any.
/// - Without `SIO_UDP_CONNRESET` and `SIO_UDP_NETRESET`: otherwise an ICMP
///   "port unreachable" for an earlier send to a peer that has gone fails
///   the next receive with `WSAECONNRESET`, and a server's receive loop
///   ends. Unix reports these only on a connected socket, and
///   [`udp_reports_refusals`] turns them back on when it connects.
///
/// A failure leaves the socket as it was.
pub(crate) fn ready_udp(sock: &socket2::Socket) {
    use windows_sys::Win32::Networking::WinSock::{SIO_UDP_CONNRESET, SIO_UDP_NETRESET};
    drop(sock.set_nonblocking(true));
    set_udp_resets(sock, &[SIO_UDP_CONNRESET, SIO_UDP_NETRESET], false);
}

/// Whether a UDP socket's receives fail when its peer's port is closed:
/// on for a connected socket, as on Unix, where the refusal comes from the
/// one peer the socket talks to. A failure leaves the socket as it was.
pub(crate) fn udp_reports_refusals(sock: &socket2::Socket, on: bool) {
    use windows_sys::Win32::Networking::WinSock::SIO_UDP_CONNRESET;
    set_udp_resets(sock, &[SIO_UDP_CONNRESET], on);
}

fn set_udp_resets(sock: &socket2::Socket, codes: &[u32], on: bool) {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::WSAIoctl;
    for &code in codes {
        let value = BOOL::from(on);
        let mut returned = 0u32;
        // SAFETY: an open socket; the input is a local of the size given,
        // and there is no output.
        unsafe {
            WSAIoctl(
                sock.as_raw_socket() as RawSock,
                code,
                (&value as *const BOOL).cast(),
                mem::size_of::<BOOL>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                None,
            )
        };
    }
}

/// `recvfrom` on a non-blocking socket into `buf`: the bytes received,
/// or -1 with the thread's last error set (`WSAEWOULDBLOCK` when nothing
/// is waiting, `WSAEMSGSIZE` when the datagram was longer than `buf` and
/// its start fills it). The sender's address goes to `from`, its length to
/// `from_len`.
///
/// # Safety
///
/// `buf` must be valid for writes of `len` bytes, `from` for a
/// `SOCKADDR_STORAGE` and `from_len` for an `i32` holding its size.
pub(crate) unsafe fn recv_from_nowait(
    sock: RawSock,
    buf: *mut u8,
    len: usize,
    from: *mut SockAddrStorage,
    from_len: *mut SockLen,
) -> isize {
    // SAFETY: as the caller guarantees.
    unsafe {
        WinSock::recvfrom(
            sock,
            buf,
            len.min(i32::MAX as usize) as i32,
            0,
            from.cast(),
            from_len,
        ) as isize
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
/// connected to itself: [`stop`](Self::stop) sets a flag and sends that
/// socket a byte, so that a wait returns. `select` reports what is true at
/// the moment it is asked, so a connection that arrives at any point is
/// seen by the next wait (an event object set by `WSAEventSelect` can be
/// reset past one).
///
/// Only the flag stops the thread. A datagram is not proof of a `stop`:
/// any program on the machine can see the socket's port, and Windows lets
/// another socket bind the wildcard address on that port (even with
/// `SO_EXCLUSIVEADDRUSE` on this one) and send from there with this
/// socket's own address as the source. One datagram would then have ended
/// the thread for good, and every listener of the shared address would
/// have stopped taking connections. So a datagram without the flag is read
/// and ignored. The bind is exclusive all the same, so that no socket can
/// take over the address itself, and with it the byte of `stop` (the flag
/// is also looked at every 100 ms).
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
            no_inherit(wake);
            let events = AcceptEvents {
                listener,
                wake,
                stopped: std::sync::atomic::AtomicBool::new(false),
            };
            let on: BOOL = 1;
            if WinSock::setsockopt(
                wake,
                WinSock::SOL_SOCKET,
                WinSock::SO_EXCLUSIVEADDRUSE,
                (&on as *const BOOL).cast(),
                mem::size_of::<BOOL>() as i32,
            ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // Non-blocking, so that `select` can read it empty.
            let mut nonblocking = 1u32;
            if WinSock::ioctlsocket(wake, WinSock::FIONBIO, &mut nonblocking) != 0 {
                return Err(std::io::Error::last_os_error());
            }
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

    /// Waits `ms` milliseconds without looking at the socket, or until
    /// [`stop`](Self::stop): for when the socket has work that cannot be
    /// taken yet, which would end a [`wait`](Self::wait) at once.
    pub(crate) fn pause(&self, ms: u32) -> AcceptWake {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return AcceptWake::Stop;
        }
        self.select(ms, false)
    }

    fn wait_once(&self, timeout: u32) -> AcceptWake {
        self.select(timeout, true)
    }

    /// `select` on the wake socket, and on the listener if `listener`.
    fn select(&self, timeout: u32, listener: bool) -> AcceptWake {
        // SAFETY: the set and the timeout are locals; both sockets are
        // open while `self` lives.
        unsafe {
            let mut read: WinSock::FD_SET = std::mem::zeroed();
            read.fd_count = 1;
            read.fd_array[0] = self.wake;
            if listener {
                read.fd_count = 2;
                read.fd_array[1] = self.listener;
            }
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
            if ready < 0 {
                // The call failed (no buffers, a closed listener): the
                // wait is not cut short, or the caller would spin.
                std::thread::sleep(std::time::Duration::from_millis(u64::from(
                    timeout.min(100),
                )));
                return AcceptWake::Timeout;
            }
            let set = &read.fd_array[..(read.fd_count as usize).min(2)];
            if ready > 0 && set.contains(&self.wake) {
                self.drain();
                if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
                    return AcceptWake::Stop;
                }
            }
            if ready > 0 && set.contains(&self.listener) {
                AcceptWake::Accept
            } else {
                AcceptWake::Timeout
            }
        }
    }

    /// Reads what waits on the wake socket, so that a datagram that came
    /// without [`stop`](Self::stop) does not keep every wait short. At most
    /// [`DRAIN_MAX`](Self::DRAIN_MAX) datagrams per wait: a program that
    /// floods the socket must not keep the thread from the listener, which
    /// the same `select` reports.
    fn drain(&self) {
        let mut buf = [0u8; 16];
        for _ in 0..Self::DRAIN_MAX {
            // SAFETY: a local buffer of the size given; the socket is open
            // while `self` lives and does not block.
            let n = unsafe { WinSock::recv(self.wake, buf.as_mut_ptr(), buf.len() as i32, 0) };
            // A longer datagram is cut to the buffer and taken all the same.
            // SAFETY: plain call.
            if n < 0 && unsafe { WinSock::WSAGetLastError() } != WinSock::WSAEMSGSIZE {
                return;
            }
        }
    }

    /// Datagrams [`drain`](Self::drain) reads per wait.
    const DRAIN_MAX: usize = 64;

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
        no_inherit(sock);
        Ok(Some(sock))
    }
}

/// Keeps a child process from inheriting `sock`, as the standard library
/// does for every socket it makes: a child holding a copy keeps the
/// connection open after this process closes it.
fn no_inherit(sock: RawSock) {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
    // SAFETY: changes a flag of a socket the caller owns. On failure the
    // socket is only inheritable, which is how `socket` made it.
    unsafe { SetHandleInformation(sock as HANDLE, HANDLE_FLAG_INHERIT, 0) };
}

/// The DNS servers and search domains Windows uses: the servers of the
/// network adapters that are up, in the order Windows lists the adapters,
/// and the primary DNS suffix followed by each adapter's own suffix.
///
/// Site-local servers (`fec0::/10`, which Windows lists on adapters with no
/// IPv6 DNS server) and link-local ones (`fe80::/10`, which need a zone
/// index) are left out.
pub(crate) fn dns_config() -> std::io::Result<(Vec<std::net::IpAddr>, Vec<String>)> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetAdaptersAddresses, GetNetworkParams, FIXED_INFO_W2KSP1, GAA_FLAG_SKIP_ANYCAST,
        GAA_FLAG_SKIP_FRIENDLY_NAME, GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST,
        IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;

    let mut search = Vec::new();
    let mut add_suffix = |suffix: String| {
        let suffix = suffix.trim_end_matches('.').to_string();
        if !suffix.is_empty() && !search.contains(&suffix) {
            search.push(suffix);
        }
    };

    // The primary DNS suffix of the machine.
    let mut len = 0u32;
    // SAFETY: a null buffer asks for the size it needs.
    if unsafe { GetNetworkParams(std::ptr::null_mut(), &mut len) } == ERROR_BUFFER_OVERFLOW {
        // `u64` units keep the buffer aligned for the structure.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        let info = buf.as_mut_ptr().cast::<FIXED_INFO_W2KSP1>();
        // SAFETY: `buf` holds `len` bytes, aligned for `FIXED_INFO_W2KSP1`.
        if unsafe { GetNetworkParams(info, &mut len) } == NO_ERROR {
            // SAFETY: the call filled the structure; `DomainName` ends in a
            // NUL within its array.
            let name = unsafe { &(*info).DomainName };
            let bytes: Vec<u8> = name
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            add_suffix(String::from_utf8_lossy(&bytes).into_owned());
        }
    }

    // The adapters. The list can grow between the two calls, so the size
    // is asked again until it fits.
    let flags = GAA_FLAG_SKIP_UNICAST
        | GAA_FLAG_SKIP_ANYCAST
        | GAA_FLAG_SKIP_MULTICAST
        | GAA_FLAG_SKIP_FRIENDLY_NAME;
    let mut len = 16 * 1024u32;
    let mut buf: Vec<u64>;
    loop {
        buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` holds `len` bytes, aligned for the structures.
        let rc = unsafe {
            GetAdaptersAddresses(
                WinSock::AF_UNSPEC as u32,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast(),
                &mut len,
            )
        };
        match rc {
            NO_ERROR => break,
            ERROR_BUFFER_OVERFLOW => continue,
            ERROR_NO_DATA => return Ok((Vec::new(), search)),
            e => return Err(std::io::Error::from_raw_os_error(e as i32)),
        }
    }
    let mut servers = Vec::new();
    let mut adapter = buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
    // SAFETY: the call filled `buf` with a list of adapters whose pointers
    // stay within it, and `buf` lives until the end of the walk.
    unsafe {
        while !adapter.is_null() {
            let a = &*adapter;
            adapter = a.Next;
            if a.OperStatus != IfOperStatusUp || a.IfType == IF_TYPE_SOFTWARE_LOOPBACK {
                continue;
            }
            let mut dns = a.FirstDnsServerAddress;
            while !dns.is_null() {
                let address = (*dns).Address;
                dns = (*dns).Next;
                let sa = address.lpSockaddr;
                if sa.is_null() {
                    continue;
                }
                let ip = match (*sa).sa_family {
                    WinSock::AF_INET => {
                        let sin = &*sa.cast::<WinSock::SOCKADDR_IN>();
                        IpAddr::V4(Ipv4Addr::from(sin.sin_addr.S_un.S_addr.to_ne_bytes()))
                    }
                    WinSock::AF_INET6 => {
                        let sin6 = &*sa.cast::<WinSock::SOCKADDR_IN6>();
                        let ip = Ipv6Addr::from(sin6.sin6_addr.u.Byte);
                        let first = ip.segments()[0];
                        if first & 0xffc0 == 0xfec0 || first & 0xffc0 == 0xfe80 {
                            continue;
                        }
                        IpAddr::V6(ip)
                    }
                    _ => continue,
                };
                if !servers.contains(&ip) {
                    servers.push(ip);
                }
            }
            if !a.DnsSuffix.is_null() {
                let mut end = 0;
                while *a.DnsSuffix.add(end) != 0 {
                    end += 1;
                }
                let suffix = std::slice::from_raw_parts(a.DnsSuffix, end);
                add_suffix(String::from_utf16_lossy(suffix));
            }
        }
    }
    Ok((servers, search))
}

/// The hosts file of Windows.
pub(crate) fn hosts_path() -> std::path::PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    std::path::Path::new(&root).join("System32\\drivers\\etc\\hosts")
}

/// Lowers the calling thread's CPU priority (compute lane threads) to below
/// normal, so the scheduler serves the workers first. Windows has no nice
/// value; a failure leaves the thread at its old priority, which is safe.
pub(crate) fn lower_thread_priority(_nice: i32) {
    use windows_sys::Win32::System::Threading::{SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    // SAFETY: plain call about the calling thread, through its pseudo-handle.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// The first Ctrl+C asks the runtimes to shut down and keeps the
    /// process; one within the same press is handled again, a later one is
    /// left to the default handler, which ends it. Listening again, with
    /// nobody listening in between, starts the count over.
    #[test]
    fn only_the_first_console_event_is_handled() {
        use windows_sys::Win32::System::Console::CTRL_C_EVENT;
        let asked = Arc::new(AtomicUsize::new(0));
        let listener = |asked: &Arc<AtomicUsize>| {
            let seen = asked.clone();
            move || {
                seen.fetch_add(1, Ordering::SeqCst);
            }
        };
        let after_same_press = || std::thread::sleep(super::SAME_PRESS * 2);
        let guard = super::SignalGuard::install(listener(&asked)).unwrap();
        // SAFETY: plain calls of the handler; nothing ends the process.
        unsafe {
            assert_eq!(super::console_event(CTRL_C_EVENT), 1, "first is handled");
            assert_eq!(
                super::console_event(CTRL_C_EVENT),
                1,
                "the same press passed on by a wrapper is handled"
            );
            assert_eq!(asked.load(Ordering::SeqCst), 1, "and asks only once");
            after_same_press();
            assert_eq!(
                super::console_event(CTRL_C_EVENT),
                0,
                "second ends the process"
            );
        }
        assert_eq!(asked.load(Ordering::SeqCst), 1);

        // A second runtime that starts listening while the first one's
        // shutdown hangs does not make the next event a first again.
        let other = super::SignalGuard::install(listener(&asked)).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { super::console_event(CTRL_C_EVENT) }, 0);
        drop(other);
        drop(guard);

        // With nobody listening in between, a new runtime starts afresh.
        let _guard = super::SignalGuard::install(listener(&asked)).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { super::console_event(CTRL_C_EVENT) }, 1);
        assert_eq!(asked.load(Ordering::SeqCst), 2);
    }

    /// A thread's event for blocking transfers is closed when the thread
    /// ends: the blocking pool starts and ends threads all the time, and an
    /// event left behind by each was one handle more for good.
    #[test]
    fn a_threads_transfer_event_is_closed_when_it_ends() {
        let event = std::thread::spawn(|| super::thread_event().unwrap() as usize)
            .join()
            .unwrap();
        assert!(super::closed_events().lock().unwrap().contains(&event));
    }

    /// The thread that hands out the connections of a shared port stops
    /// for `stop` only. A datagram that reaches its wake socket some other
    /// way is read and ignored: one sent by the socket itself, which is
    /// what another socket can forge, and, where Windows lets a socket bind
    /// the wildcard address on the wake socket's port (real Windows does,
    /// Wine does not), one forged from there.
    #[test]
    fn only_stop_ends_a_wait_for_connections() {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let events = super::AcceptEvents::new(listener.as_raw_socket() as super::RawSock).unwrap();

        // SAFETY: plain data, filled in by the call.
        let mut addr: WinSock::SOCKADDR_IN = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<WinSock::SOCKADDR_IN>() as i32;
        // SAFETY: an open socket; the address is a local of the size given.
        let rc = unsafe {
            WinSock::getsockname(
                events.wake,
                (&mut addr as *mut WinSock::SOCKADDR_IN).cast(),
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        let port = u16::from_be(addr.sin_port);
        if let Ok(forger) = std::net::UdpSocket::bind(("0.0.0.0", port)) {
            for _ in 0..3 {
                forger.send_to(&[7], ("127.0.0.1", port)).unwrap();
                assert_eq!(events.wait(Some(20)), super::AcceptWake::Timeout);
            }
        }

        for _ in 0..3 {
            // SAFETY: one byte from a local, to the socket's own address.
            let sent = unsafe { WinSock::send(events.wake, [7u8].as_ptr(), 1, 0) };
            assert_eq!(sent, 1);
            assert_eq!(events.wait(Some(20)), super::AcceptWake::Timeout);
            assert_eq!(events.pause(20), super::AcceptWake::Timeout);
        }
        // A flood of them does not hide a connection: the listener is
        // reported by the same wait.
        for _ in 0..4 * super::AcceptEvents::DRAIN_MAX {
            // SAFETY: as above.
            unsafe { WinSock::send(events.wake, [7u8].as_ptr(), 1, 0) };
        }
        let _peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let started = std::time::Instant::now();
        while events.wait(Some(5_000)) != super::AcceptWake::Accept {
            assert!(
                started.elapsed() < std::time::Duration::from_secs(5),
                "never saw the connection"
            );
        }

        events.stop();
        assert_eq!(events.wait(None), super::AcceptWake::Stop);
        assert_eq!(events.pause(20), super::AcceptWake::Stop);
    }
}
