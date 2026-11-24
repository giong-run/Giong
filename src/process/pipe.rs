//! Pipes, and the process's own standard input and output, on the driver.

use std::io;
use std::marker::PhantomData;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::process::Stdio;

use crate::buf::{IoBuf, IoBufMut};
use crate::driver::Io;
use crate::net::os_err;
use crate::op::Op;
use crate::runtime;

/// Bytes [`PipeReader::read_to_end`] reads at a time.
const CHUNK: usize = 64 * 1024;

/// One end of a pipe, or a standard stream, as a descriptor the driver
/// reads or writes.
struct End {
    fd: RawFd,
    /// The process's own stdin, stdout or stderr (a copy of 0, 1 or 2): its
    /// open file is shared with the parent process and with `std::io`, so
    /// it is never made non-blocking (see `Driver::keep_blocking`).
    shared: bool,
    _not_send: PhantomData<*const ()>,
}

impl End {
    fn new(fd: OwnedFd, shared: bool) -> Self {
        Self {
            fd: fd.into_raw_fd(),
            shared,
            _not_send: PhantomData,
        }
    }

    fn prepare(&self) {
        if self.shared {
            runtime::with_worker(|w| w.driver.borrow_mut().keep_blocking(self.fd));
        }
    }

    /// Waits until the descriptor is ready, after `EAGAIN` from a descriptor
    /// that someone else made non-blocking.
    async fn ready(&self, write: bool) -> io::Result<()> {
        let io = if write {
            Io::Writable { fd: self.fd }
        } else {
            Io::Readable { fd: self.fd }
        };
        // SAFETY: the operation points at no memory.
        let (res, _, ()) = unsafe { Op::new(io, ()) }.await;
        if res < 0 {
            Err(os_err(res))
        } else {
            Ok(())
        }
    }

    async fn read<B: IoBufMut>(&self, mut buf: B) -> (io::Result<usize>, B) {
        self.prepare();
        loop {
            let cap = buf.capacity();
            let io = Io::Read {
                fd: self.fd,
                buf: buf.as_mut_ptr(),
                len: cap.min(u32::MAX as usize) as u32,
                // The descriptor's own position: a pipe has none, and a
                // regular file given as stdin is read from where it is.
                offset: u64::MAX,
            };
            // SAFETY: an `IoBufMut`'s bytes stay put when it moves.
            let (res, _, b) = unsafe { Op::new(io, buf) }.await;
            buf = b;
            if res == -libc::EAGAIN {
                if let Err(e) = self.ready(false).await {
                    return (Err(e), buf);
                }
                continue;
            }
            if res < 0 {
                return (Err(os_err(res)), buf);
            }
            // SAFETY: the kernel wrote `res` bytes at the start of the buffer.
            unsafe { buf.set_init(res as usize) };
            return (Ok(res as usize), buf);
        }
    }

    async fn write_from<B: IoBuf>(&self, mut buf: B, from: usize) -> (io::Result<usize>, B) {
        self.prepare();
        loop {
            let len = buf.len() - from;
            let io = Io::Write {
                fd: self.fd,
                // SAFETY: `from <= len`, so the pointer stays in the buffer.
                buf: unsafe { buf.as_ptr().add(from) },
                len: len.min(u32::MAX as usize) as u32,
                offset: u64::MAX,
            };
            // SAFETY: an `IoBuf`'s bytes stay put when it moves.
            let (res, _, b) = unsafe { Op::new(io, buf) }.await;
            buf = b;
            if res == -libc::EAGAIN {
                if let Err(e) = self.ready(true).await {
                    return (Err(e), buf);
                }
                continue;
            }
            if res < 0 {
                return (Err(os_err(res)), buf);
            }
            return (Ok(res as usize), buf);
        }
    }

    async fn write_all<B: IoBuf>(&self, mut buf: B) -> (io::Result<()>, B) {
        let mut written = 0;
        while written < buf.len() {
            let (res, b) = self.write_from(buf, written).await;
            buf = b;
            match res {
                Ok(0) => return (Err(io::ErrorKind::WriteZero.into()), buf),
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return (Err(e), buf),
            }
        }
        (Ok(()), buf)
    }

    /// Hands the descriptor on, unclosed.
    fn into_owned(self) -> OwnedFd {
        let fd = self.fd;
        std::mem::forget(self);
        runtime::try_with_worker(|w| {
            if let Ok(mut driver) = w.driver.try_borrow_mut() {
                driver.release_fd(fd);
            }
        });
        // SAFETY: we owned `fd` and give it up here, unclosed.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }
}

impl Drop for End {
    fn drop(&mut self) {
        let fd = self.fd;
        let closed = runtime::try_with_worker(|w| match w.driver.try_borrow_mut() {
            Ok(mut driver) => driver.close_fd(fd),
            Err(_) => w.defer_close(fd),
        })
        .is_some();
        if !closed {
            // SAFETY: we own `fd` and close it exactly once.
            unsafe { libc::close(fd) };
        }
    }
}

/// The reading end of a pipe: a child's stdout or stderr
/// ([`Child::stdout`](super::Child::stdout)), the process's own stdin
/// ([`stdin`]), or one end of a [`pipe`].
///
/// Reads go through the worker's io_uring, and on the epoll driver through
/// epoll (the process's own stdin through a blocking thread, so that it is
/// never made non-blocking for the processes that share it).
pub struct PipeReader {
    end: End,
}

impl PipeReader {
    /// Wraps a descriptor open for reading: a pipe, a FIFO, a character
    /// device, or a regular file (read from its current position).
    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self {
            end: End::new(fd, false),
        }
    }

    /// Reads some bytes into `buf`, returning how many. **0 means end of
    /// file: every writer has closed its end** (for a child's stdout, the
    /// child has exited or closed it).
    ///
    /// Not cancel safe: if the future is dropped, bytes the read had already
    /// taken from the pipe are lost.
    pub async fn read<B: IoBufMut>(&self, buf: B) -> (io::Result<usize>, B) {
        self.end.read(buf).await
    }

    /// Reads until end of file, appending to `buf`; returns the number of
    /// bytes read. Unbounded: use it for output whose size you trust, such
    /// as a tool's result, or read in a loop with [`read`](Self::read).
    ///
    /// Not cancel safe, as [`read`](Self::read).
    pub async fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut chunk = Vec::with_capacity(CHUNK);
        let mut total = 0;
        loop {
            chunk.clear();
            let (res, c) = self.end.read(chunk).await;
            chunk = c;
            match res {
                Ok(0) => return Ok(total),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    total += n;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// The descriptor, unclosed, to hand on (as a child's stdin, with
    /// `Stdio::from`, say).
    pub fn into_owned_fd(self) -> OwnedFd {
        self.end.into_owned()
    }
}

/// The writing end of a pipe: a child's stdin
/// ([`Child::stdin`](super::Child::stdin)), the process's own stdout or
/// stderr ([`stdout`], [`stderr`]), or one end of a [`pipe`].
///
/// Dropping it closes it, which the reader sees as end of file.
pub struct PipeWriter {
    end: End,
}

impl PipeWriter {
    /// Wraps a descriptor open for writing: a pipe, a FIFO, a character
    /// device, or a regular file (written at its current position).
    pub fn from_owned_fd(fd: OwnedFd) -> Self {
        Self {
            end: End::new(fd, false),
        }
    }

    /// Writes some bytes of `buf`, returning how many. Writing to a pipe
    /// whose readers have all closed fails with `BrokenPipe`.
    ///
    /// Not cancel safe: if the future is dropped, some bytes may have been
    /// written without the caller learning how many.
    pub async fn write<B: IoBuf>(&self, buf: B) -> (io::Result<usize>, B) {
        self.end.write_from(buf, 0).await
    }

    /// Writes all of `buf`. The buffer is handed back so it can be reused.
    ///
    /// Not cancel safe: if the future is dropped, part of the data may have
    /// been written.
    pub async fn write_all<B: IoBuf>(&self, buf: B) -> (io::Result<()>, B) {
        self.end.write_all(buf).await
    }

    /// The descriptor, unclosed, to hand on (as a child's stdout, with
    /// `Stdio::from`, say).
    pub fn into_owned_fd(self) -> OwnedFd {
        self.end.into_owned()
    }
}

macro_rules! fd_traits {
    ($t:ty) => {
        impl AsRawFd for $t {
            fn as_raw_fd(&self) -> RawFd {
                self.end.fd
            }
        }

        impl AsFd for $t {
            fn as_fd(&self) -> BorrowedFd<'_> {
                // SAFETY: the descriptor is open while `self` lives.
                unsafe { BorrowedFd::borrow_raw(self.end.fd) }
            }
        }

        impl From<$t> for Stdio {
            fn from(end: $t) -> Stdio {
                Stdio::from(end.into_owned_fd())
            }
        }

        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct(stringify!($t))
                    .field("fd", &self.end.fd)
                    .finish()
            }
        }
    };
}

fd_traits!(PipeReader);
fd_traits!(PipeWriter);

/// Creates a pipe: bytes written to the [`PipeWriter`] come out of the
/// [`PipeReader`]. Both ends are closed on `exec`; hand one to a child with
/// `Stdio::from(end)`.
pub fn pipe() -> io::Result<(PipeReader, PipeWriter)> {
    let mut fds = [0; 2];
    // SAFETY: a valid pointer to two descriptors.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` returned two descriptors we now own.
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    Ok((PipeReader::from_owned_fd(r), PipeWriter::from_owned_fd(w)))
}

/// A copy of one of the process's standard descriptors, sharing its open
/// file.
fn standard(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: `fcntl` duplicates the descriptor or fails; the copy is ours.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new descriptor we own.
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

/// The process's standard input, for reading without blocking the worker:
/// a pipe or a terminal through io_uring, and on the epoll driver through a
/// blocking thread.
///
/// Each call makes a new reader on a copy of descriptor 0. Read from one
/// reader at a time: two would each get part of the input. Mixing it with
/// `std::io::stdin()`, which buffers, loses input the same way.
///
/// The descriptor is never made non-blocking, since its open file is
/// shared with the parent process (the shell) and with `std::io`. On the
/// epoll driver a read in progress holds a blocking thread until input
/// arrives, and cannot be called off: one read waiting at a time is the
/// way to use it.
pub fn stdin() -> io::Result<PipeReader> {
    Ok(PipeReader {
        end: End::new(standard(0)?, true),
    })
}

/// The process's standard output, for writing without blocking the worker.
/// Writes go out unbuffered, in the order they complete; interleaving them
/// with `println!` (which buffers) mixes lines unpredictably.
///
/// Each call makes a new writer on a copy of descriptor 1, never made
/// non-blocking (see [`stdin`]).
pub fn stdout() -> io::Result<PipeWriter> {
    Ok(PipeWriter {
        end: End::new(standard(1)?, true),
    })
}

/// The process's standard error, as [`stdout`] for descriptor 2.
pub fn stderr() -> io::Result<PipeWriter> {
    Ok(PipeWriter {
        end: End::new(standard(2)?, true),
    })
}
