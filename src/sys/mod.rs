//! The system calls giong makes outside its I/O drivers, one module per
//! operating system.
//!
//! Everything here has the same shape on every system: the runtime, the
//! readiness driver and the `fs` and `net` modules call `sys::…` and never
//! name an OS-specific function themselves. On Linux each function is the
//! call giong has always made (through `libc`, so the simulated kernel in
//! `sim/` keeps seeing the same symbols); on macOS it is the closest
//! equivalent, and where none exists the difference is written next to it.
//!
//! See `docs/design/portability.md` for the boundary and what is still
//! missing for Windows.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) use linux::*;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::*;

use std::io;
use std::os::fd::RawFd;

/// A socket, as operations name it (`Io`, `MultiIo`): a descriptor on Unix,
/// a `SOCKET` on Windows.
#[cfg(unix)]
pub(crate) type RawSock = RawFd;

/// A file, pipe or device, as operations name it: a descriptor on Unix, a
/// `HANDLE` on Windows. On Unix it is the same type as [`RawSock`], and
/// code may pass one for the other; on Windows the two are closed, read
/// and written by different calls, so an operation says which it takes.
#[cfg(unix)]
pub(crate) type RawFile = RawFd;

/// A socket address as the system reads it (`SOCKADDR` on Windows).
#[cfg(unix)]
pub(crate) type SockAddr = libc::sockaddr;

/// The length of a socket address (`i32` on Windows).
#[cfg(unix)]
pub(crate) type SockLen = libc::socklen_t;

/// Room for any socket address (`SOCKADDR_STORAGE` on Windows).
#[cfg(unix)]
#[allow(dead_code)] // The Unix drivers still name `libc::sockaddr_storage`.
pub(crate) type SockAddrStorage = libc::sockaddr_storage;

/// The header of a `sendmsg` or `recvmsg`: buffers, address and control
/// data (`WSAMSG` on Windows, whose fields have other names and order, so
/// code that fills one in is per system).
#[cfg(unix)]
pub(crate) type MsgHdr = libc::msghdr;

/// One unit of a path as the system takes it: a byte of a NUL-terminated
/// string on Unix, a UTF-16 unit on Windows.
#[cfg(unix)]
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
    read: std::os::fd::OwnedFd,
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    write: std::os::fd::OwnedFd,
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
        ring(self.write_fd());
    }

    /// Resets the bell after it was seen. Never blocks.
    pub(crate) fn drain(&self) {
        drain(self.read_fd());
    }

    /// Gives up ownership: the descriptors stay open for ever (the signal
    /// bell, which a handler may write to at any time).
    pub(crate) fn leak(self) -> (RawFd, RawFd) {
        let fds = (self.read_fd(), self.write_fd());
        std::mem::forget(self);
        fds
    }
}
