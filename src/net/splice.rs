//! Moving bytes from one stream to another inside the kernel: `splice(2)`
//! through a pipe, the way HAProxy forwards bodies.
//!
//! Each call is a non-blocking `splice(2)` made by the worker when the
//! socket is ready, as `send_file` does with `sendfile(2)`. io_uring has an
//! `IORING_OP_SPLICE`, but the kernel runs it on a blocking io-wq thread for
//! as long as it waits, one thread per idle connection; readiness costs no
//! thread, and both drivers share this path.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use super::{os_err, StreamSocket};
use crate::driver::Io;
use crate::error::{raise, Code};
use crate::op::Op;
use crate::runtime;

/// Bytes moved before the task lets the worker's other tasks run.
const SPLICE_YIELD: u64 = 512 * 1024;
/// The pipe's capacity, asked for once per call (`F_SETPIPE_SZ`). Larger
/// than the default 64 KiB, so one pass moves what a fast socket holds; an
/// unprivileged process may ask up to `/proc/sys/fs/pipe-max-size` (1 MiB
/// by default), and a refusal keeps the default.
const PIPE_SIZE: libc::c_int = 1 << 20;

/// A stream that [`TcpStream::splice_to`](super::TcpStream::splice_to) and
/// [`UnixStream::splice_to`](super::UnixStream::splice_to) can move bytes
/// to or from: [`TcpStream`](super::TcpStream) and
/// [`UnixStream`](super::UnixStream). Sealed: it cannot be implemented
/// outside giong.
pub trait SpliceStream: sealed::Sealed {}

pub(crate) mod sealed {
    pub trait Sealed {
        #[allow(private_interfaces)]
        fn stream(&self) -> &super::StreamSocket;
    }
}

/// A pipe of the kernel's, both ends non-blocking.
struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Pipe {
    fn new() -> io::Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: `pipe2` writes two descriptors into the array it is given.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: two fresh descriptors we own.
        let pipe = unsafe {
            Self {
                read: OwnedFd::from_raw_fd(fds[0]),
                write: OwnedFd::from_raw_fd(fds[1]),
            }
        };
        // SAFETY: plain fcntl on a descriptor we own; failure leaves the
        // default size.
        unsafe { libc::fcntl(fds[1], libc::F_SETPIPE_SZ, PIPE_SIZE) };
        Ok(pipe)
    }
}

/// One non-blocking `splice(2)` of up to `len` bytes: `Ok(None)` if it
/// would block.
fn splice(from: RawFd, to: RawFd, len: usize) -> io::Result<Option<usize>> {
    loop {
        // SAFETY: a system call on descriptors the caller keeps open, with
        // no offsets (both are a socket or a pipe).
        let n = unsafe {
            libc::splice(
                from,
                std::ptr::null_mut(),
                to,
                std::ptr::null_mut(),
                len,
                libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK,
            )
        };
        if n >= 0 {
            return Ok(Some(n as usize));
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EAGAIN) => return Ok(None),
            _ => return Err(err),
        }
    }
}

/// Waits until `fd` can be read (`readable`) or written.
async fn ready(fd: RawFd, readable: bool) -> io::Result<()> {
    let io = if readable {
        Io::Readable { fd }
    } else {
        Io::Writable { fd }
    };
    // SAFETY: the operation has no pointers.
    let (res, _, ()) = unsafe { Op::new(io, ()) }.await;
    if res < 0 {
        return Err(os_err(res));
    }
    Ok(())
}

impl SpliceStream for super::TcpStream {}
impl sealed::Sealed for super::TcpStream {
    #[allow(private_interfaces)]
    fn stream(&self) -> &StreamSocket {
        &self.inner
    }
}
impl SpliceStream for super::UnixStream {}
impl sealed::Sealed for super::UnixStream {
    #[allow(private_interfaces)]
    fn stream(&self) -> &StreamSocket {
        self.stream_socket()
    }
}

impl StreamSocket {
    /// See [`TcpStream::splice_to`](super::TcpStream::splice_to).
    pub(crate) async fn splice_to(&self, to: &StreamSocket, limit: u64) -> io::Result<u64> {
        if runtime::with_worker(|w| self.recv.is_active(w)) {
            return Err(raise(
                Code::MixedReads,
                "splice_to cannot be used after recv on the same stream",
                "use recv (buffers from the runtime's pool) or splice_to (bytes moved inside \
                 the kernel) on one stream, not both: data recv has taken in already would \
                 be skipped. Forward what recv returned with write_all, or call splice_to \
                 before the first recv.",
            ));
        }
        self.set_nonblocking()?;
        to.set_nonblocking()?;
        let (from, dest) = (self.socket().as_raw_fd(), to.socket().as_raw_fd());
        let pipe = Pipe::new()?;
        let mut moved = 0u64;
        let mut since_yield = 0u64;
        while moved < limit {
            let want = (limit - moved).min(PIPE_SIZE as u64) as usize;
            let n = match splice(from, pipe.write.as_raw_fd(), want)? {
                Some(0) => break,
                Some(n) => n,
                None => {
                    ready(from, true).await?;
                    continue;
                }
            };
            // Out of the pipe before more goes in: what it holds left
            // `self` and must reach `to`.
            let mut left = n;
            while left > 0 {
                match splice(pipe.read.as_raw_fd(), dest, left)? {
                    Some(m) => {
                        left -= m;
                        moved += m as u64;
                    }
                    None => ready(dest, false).await?,
                }
            }
            since_yield += n as u64;
            if since_yield >= SPLICE_YIELD {
                since_yield = 0;
                crate::yield_now().await;
            }
        }
        Ok(moved)
    }
}
