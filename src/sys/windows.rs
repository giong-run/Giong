//! Windows: what the runtime asks of the system outside its driver.
//!
//! Sockets and files are different things here (`SOCKET` and `HANDLE`),
//! addresses and message headers are Winsock's, and paths are UTF-16. The
//! wake-up of a worker is not in this file: on Windows it is a packet
//! posted to the worker's completion port, which the driver owns
//! (`driver::iocp`).

use std::ffi::c_void;

use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::{SOCKADDR, SOCKADDR_STORAGE, SOCKET, WSAMSG};
use windows_sys::Win32::Storage::FileSystem::FlushFileBuffers;
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

#[allow(dead_code)]
const _: fn() = || {
    // `Io` is `Copy` and holds these by value.
    fn copy<T: Copy>() {}
    copy::<RawSock>();
    copy::<RawFile>();
    copy::<*const c_void>();
};
