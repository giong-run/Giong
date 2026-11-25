//! The socket calls giong-tls makes itself, outside giong's driver: sends
//! that do not wait, for records that must go out between the operations a
//! task runs (key updates, session tickets, alerts, `close_notify`).
//!
//! On Unix a socket is a file descriptor; on Windows it is a `SOCKET`, which
//! giong's driver puts in non-blocking mode when it first uses it, so the
//! stream has been through the driver (the handshake) before any call here.

use std::io;

#[cfg(unix)]
pub(crate) use std::os::fd::{AsRawFd, RawFd};

/// A socket, as the calls here name it.
#[cfg(windows)]
pub(crate) type RawFd = std::os::windows::io::RawSocket;

/// The socket of a stream, under the name the Unix code uses.
#[cfg(windows)]
pub(crate) trait AsRawFd {
    /// The stream's socket.
    fn as_raw_fd(&self) -> RawFd;
}

#[cfg(windows)]
impl<T: std::os::windows::io::AsRawSocket> AsRawFd for T {
    fn as_raw_fd(&self) -> RawFd {
        self.as_raw_socket()
    }
}

/// The flag that makes a send to a closed peer fail with `EPIPE` instead of
/// raising `SIGPIPE`. macOS has none: giong's sockets carry `SO_NOSIGPIPE`.
#[cfg(target_os = "linux")]
pub(crate) const NO_SIGNAL: libc::c_int = libc::MSG_NOSIGNAL;
#[cfg(target_os = "macos")]
pub(crate) const NO_SIGNAL: libc::c_int = 0;

/// Sends from `buf` without waiting and returns how many bytes the socket
/// took. A full socket fails with [`io::ErrorKind::WouldBlock`].
///
/// `more`: more bytes follow at once (Linux holds them for the next segment
/// with `MSG_MORE`; elsewhere it changes nothing).
pub(crate) fn send_now(fd: RawFd, buf: &[u8], more: bool) -> io::Result<usize> {
    #[cfg(unix)]
    {
        #[cfg(target_os = "linux")]
        let flags = libc::MSG_DONTWAIT | NO_SIGNAL | if more { libc::MSG_MORE } else { 0 };
        #[cfg(target_os = "macos")]
        let flags = {
            let _ = more;
            libc::MSG_DONTWAIT | NO_SIGNAL
        };
        // SAFETY: `buf` is valid for reads of its length for the call.
        let n = unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), flags) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Networking::WinSock::{send, WSAGetLastError, SOCKET_ERROR};
        let _ = more;
        // At most 1 MiB, as giong's driver sends on a stream: Winsock takes
        // the whole of a send whenever its buffer is not yet full. The rest
        // goes later, as after any short send.
        let len = buf.len().min(1 << 20) as i32;
        // SAFETY: `buf` is valid for reads of `len` bytes for the call. The
        // socket is non-blocking (see the module documentation).
        let n = unsafe { send(fd as usize, buf.as_ptr(), len, 0) };
        if n == SOCKET_ERROR {
            // SAFETY: reads the calling thread's last Winsock error.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        Ok(n as usize)
    }
}
