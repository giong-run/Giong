//! Files on io_uring.
//!
//! Unlike sockets, regular files are never "ready" in the `epoll` sense, which
//! is why other runtimes send file I/O to a pool of blocking threads. With
//! io_uring it goes through the same ring as everything else: opens, reads,
//! writes and syncs are submitted and completed in the same batches as
//! network I/O, and the worker thread never blocks on the disk.
//!
//! On the epoll driver (see [`DriverKind`](crate::DriverKind)) these
//! operations run on a pool of blocking threads instead, with the same API
//! and the same cancellation rules.
//!
//! ```no_run
//! use giong::fs::{self, File};
//!
//! # async fn demo() -> std::io::Result<()> {
//! // Whole files.
//! fs::write("hello.txt", "xin chào").await?;
//! let text = fs::read("hello.txt").await?;
//! assert_eq!(text, "xin chào".as_bytes());
//!
//! // Reads and writes at an offset.
//! let file = File::options().read(true).write(true).open("hello.txt").await?;
//! let (res, _) = file.write_all_at("XIN", 0).await;
//! res?;
//! let (res, buf) = file.read_at(Vec::with_capacity(3), 0).await;
//! assert_eq!((res?, &buf[..]), (3, &b"XIN"[..]));
//! file.sync_all().await?;
//! file.close().await?;
//! fs::remove_file("hello.txt").await
//! # }
//! ```
//!
//! A [`File`] has no cursor: every read and write names its offset, so one
//! file can be shared by many tasks without them disturbing each other.
//!
//! A path that comes from outside (a request, an upload, a configuration
//! file) is opened through a [`Root`], which keeps it beneath one directory.
//!
//! [`read_dir`] lists a directory. It is the one operation here that does not
//! go through the ring, as io_uring cannot read directories; its
//! documentation says what that costs.

use std::ffi::{CString, OsStr, OsString};
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::buf::{IoBuf, IoBufMut};
use crate::driver::Io;
use crate::error::{raise, Code};
use crate::kbox::KernelBox;
use crate::op::Op;
use crate::runtime;
use crate::sys;

fn os_err(res: i32) -> io::Error {
    io::Error::from_raw_os_error(-res)
}

/// `path` as a C string for the kernel, in a [`KernelBox`] so that the
/// pointer to it stays valid while the operation's resources move.
fn c_path(path: &Path) -> io::Result<KernelBox<CString>> {
    CString::new(path.as_os_str().as_bytes())
        .map(KernelBox::path)
        .map_err(|_| {
            raise(
                Code::InvalidPath,
                format!("the path {path:?} contains a NUL byte"),
                "remove the NUL byte; Linux paths cannot contain one.",
            )
        })
}

/// Runs an operation whose only resource is one path.
async fn path_op(path: KernelBox<CString>, io: Io) -> io::Result<i32> {
    // SAFETY: the operation's only pointer is to the path in its
    // `KernelBox`.
    let (res, _, _path) = unsafe { Op::new(io, path) }.await;
    if res < 0 {
        Err(os_err(res))
    } else {
        Ok(res)
    }
}

/// How a file is opened; see [`File::options`].
///
/// The options are those of [`std::fs::OpenOptions`].
#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    mode: u32,
    custom_flags: i32,
}

impl OpenOptions {
    /// All options off; permissions `0o666` (before the umask) for new files.
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            mode: 0o666,
            custom_flags: 0,
        }
    }

    /// Open for reading.
    pub fn read(mut self, read: bool) -> Self {
        self.read = read;
        self
    }

    /// Open for writing.
    pub fn write(mut self, write: bool) -> Self {
        self.write = write;
        self
    }

    /// Every write goes to the end of the file, whatever offset it names.
    pub fn append(mut self, append: bool) -> Self {
        self.append = append;
        self
    }

    /// Cut the file to length zero when opening it.
    pub fn truncate(mut self, truncate: bool) -> Self {
        self.truncate = truncate;
        self
    }

    /// Create the file if it does not exist.
    pub fn create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Create the file, failing if it already exists.
    pub fn create_new(mut self, create_new: bool) -> Self {
        self.create_new = create_new;
        self
    }

    /// Permission bits for a newly created file.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = mode;
        self
    }

    /// Extra `open(2)` flags, for example `libc::O_DIRECT`. The access mode
    /// bits (`O_RDONLY`, `O_WRONLY`, `O_RDWR`) are ignored: they come from
    /// [`read`](Self::read), [`write`](Self::write) and
    /// [`append`](Self::append), as in the standard library.
    pub fn custom_flags(mut self, flags: i32) -> Self {
        self.custom_flags = flags;
        self
    }

    fn flags(&self) -> io::Result<i32> {
        let access = match (self.read, self.write, self.append) {
            (true, false, false) => libc::O_RDONLY,
            (false, true, false) => libc::O_WRONLY,
            (true, true, false) => libc::O_RDWR,
            (false, _, true) => libc::O_WRONLY | libc::O_APPEND,
            (true, _, true) => libc::O_RDWR | libc::O_APPEND,
            (false, false, false) => {
                return Err(invalid_options(
                    "the open options ask for neither read, write nor append access",
                    "add .read(true), .write(true) or .append(true)",
                ))
            }
        };
        let writable = self.write || self.append;
        if !writable && (self.truncate || self.create || self.create_new) {
            let what = if self.truncate {
                "truncate"
            } else if self.create_new {
                "create_new"
            } else {
                "create"
            };
            return Err(invalid_options(
                format!("the open options ask for {what} without write access"),
                format!("add .write(true) (or .append(true)), or remove .{what}(true)"),
            ));
        }
        if self.append && self.truncate && !self.create_new {
            return Err(invalid_options(
                "the open options ask for both append and truncate",
                "to empty the file and then append, open it with .write(true).truncate(true) \
                 and write in order; to keep what it has, remove .truncate(true)",
            ));
        }
        let creation = match (self.create, self.truncate, self.create_new) {
            (_, _, true) => libc::O_CREAT | libc::O_EXCL,
            (false, false, false) => 0,
            (true, false, false) => libc::O_CREAT,
            (false, true, false) => libc::O_TRUNC,
            (true, true, false) => libc::O_CREAT | libc::O_TRUNC,
        };
        // The access mode comes from read/write/append alone, as in the
        // standard library: custom flags cannot turn a read-only open into a
        // writable one.
        Ok(access | creation | libc::O_CLOEXEC | (self.custom_flags & !libc::O_ACCMODE))
    }

    /// Opens `path` with these options.
    ///
    /// If the future is dropped once the open has started, the descriptor
    /// it may still produce is closed. What the open did to the file stays
    /// done: a dropped `create` or `truncate` may have created or emptied
    /// the file, in any runtime.
    ///
    /// Not cancel safe when the options create or truncate (see above); cancel
    /// safe otherwise: a dropped open leaves no descriptor behind.
    pub async fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        let flags = self.flags()?;
        let path = c_path(path.as_ref())?;
        let io = Io::OpenAt {
            path: path.c_ptr(),
            flags,
            mode: self.mode,
        };
        let fd = path_op(path, io).await?;
        Ok(File::from_fd(fd))
    }

    /// Opens `path` beneath `root` with these options, refusing a path
    /// that could lead out of it (see [`Root`]).
    ///
    /// Cancel safe as [`open`](Self::open) is.
    pub async fn open_in<P: AsRef<Path>>(&self, root: &Root, path: P) -> io::Result<File> {
        let flags = self.flags()?;
        let fd = root.open_beneath(path.as_ref(), flags, self.mode).await?;
        Ok(File::from_fd(fd))
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Refuses an offset Linux cannot take. io_uring reads `u64::MAX` (-1) as
/// "the file's current position", so without this check such an offset would
/// read or write wherever the descriptor's position happens to be on
/// io_uring, and fail with `EINVAL` on epoll.
#[cold]
fn invalid_options(
    message: impl Into<std::borrow::Cow<'static, str>>,
    fix: impl Into<std::borrow::Cow<'static, str>>,
) -> io::Error {
    raise(Code::InvalidOpenOptions, message, fix)
}

fn check_offset(offset: u64) -> io::Result<()> {
    if offset > i64::MAX as u64 {
        return Err(offset_out_of_range(offset));
    }
    Ok(())
}

#[cold]
fn offset_out_of_range(offset: u64) -> io::Error {
    raise(
        Code::OffsetOutOfRange,
        format!(
            "file offset {offset} is above {}, the largest Linux has",
            i64::MAX
        ),
        "check the arithmetic that computed the offset for a wrap-around. To write at the end \
         of a file, open it with OpenOptions::append(true).",
    )
}

/// An open file.
///
/// All methods take `&self` and name the offset they work at, so a file can
/// be shared (e.g. in an `Rc`) by any number of tasks.
pub struct File {
    fd: RawFd,
    _not_send: PhantomData<*const ()>,
}

impl File {
    /// Opens an existing file for reading.
    ///
    /// Cancel safe: if the future is dropped, the descriptor the open may still
    /// produce is closed, and nothing else changes.
    pub async fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        OpenOptions::new().read(true).open(path).await
    }

    /// Opens a file for writing, creating it if needed and emptying it if it
    /// exists.
    ///
    /// Not cancel safe: if the future is dropped, the file may have been
    /// created or emptied (the descriptor is closed either way).
    pub async fn create<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
    }

    /// Options for opening a file in any other way.
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    /// Wraps a file opened by the standard library.
    pub fn from_std(file: std::fs::File) -> Self {
        Self::from_fd(file.into_raw_fd())
    }

    fn from_fd(fd: RawFd) -> Self {
        Self {
            fd,
            _not_send: PhantomData,
        }
    }

    /// Reads at `offset` into `buf` (from its start, up to its capacity),
    /// returning the buffer with its length set to the bytes read. `Ok(0)`
    /// means `offset` is at or past the end of the file.
    ///
    /// Like `read(2)`, this may return fewer bytes than there is room for.
    ///
    /// Dropping the future is harmless for a regular file. If the descriptor
    /// is a pipe or a device, a read that is dropped while in flight may
    /// still take data that arrives before the kernel has processed the
    /// cancellation, and that data is lost.
    ///
    /// Fails with `GIONG-E0202` if `offset` is above `i64::MAX`, the largest
    /// file offset Linux has.
    ///
    /// Cancel safe for a regular file, as said above; not for a pipe or a
    /// device.
    pub async fn read_at<B: IoBufMut>(&self, mut buf: B, offset: u64) -> (io::Result<usize>, B) {
        if let Err(e) = check_offset(offset) {
            return (Err(e), buf);
        }
        let len = buf.capacity().min(u32::MAX as usize) as u32;
        // From the page cache at once when it is there (see `read_cached`).
        if let Some(res) = crate::driver::read_cached(self.fd, buf.as_mut_ptr(), len, offset) {
            // SAFETY: the kernel wrote `res` bytes at the start of the buffer.
            unsafe { buf.set_init(res as usize) };
            return (Ok(res as usize), buf);
        }
        let io = Io::Read {
            fd: self.fd,
            buf: buf.as_mut_ptr(),
            len,
            offset,
        };
        // SAFETY: an `IoBufMut`'s bytes stay put when it moves.
        let (res, _, mut buf) = unsafe { Op::new(io, buf) }.await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
        // SAFETY: the kernel wrote `res` bytes at the start of the buffer.
        unsafe { buf.set_init(res as usize) };
        (Ok(res as usize), buf)
    }

    /// Writes some bytes of `buf` at `offset`, returning how many were
    /// written.
    ///
    /// Fails with `GIONG-E0202` if `offset` is above `i64::MAX`, the largest
    /// file offset Linux has.
    ///
    /// Not cancel safe: if the future is dropped, some bytes may have been
    /// written without the caller learning how many.
    pub async fn write_at<B: IoBuf>(&self, buf: B, offset: u64) -> (io::Result<usize>, B) {
        self.write_from(buf, 0, offset).await
    }

    /// Writes all of `buf` at `offset`. The buffer is handed back so it can be
    /// reused.
    ///
    /// Not cancel safe: if the future is dropped, part of the data may have
    /// been written.
    pub async fn write_all_at<B: IoBuf>(&self, mut buf: B, offset: u64) -> (io::Result<()>, B) {
        let mut written = 0;
        while written < buf.len() {
            let Some(at) = offset.checked_add(written as u64) else {
                return (Err(offset_out_of_range(offset)), buf);
            };
            let (res, b) = self.write_from(buf, written, at).await;
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

    async fn write_from<B: IoBuf>(
        &self,
        buf: B,
        skip: usize,
        offset: u64,
    ) -> (io::Result<usize>, B) {
        debug_assert!(skip <= buf.len());
        if let Err(e) = check_offset(offset) {
            return (Err(e), buf);
        }
        let len = (buf.len() - skip).min(u32::MAX as usize) as u32;
        // SAFETY: `skip <= len`, so the pointer stays within the buffer.
        let ptr = unsafe { buf.as_ptr().add(skip) };
        let io = Io::Write {
            fd: self.fd,
            buf: ptr,
            len,
            offset,
        };
        // SAFETY: `ptr` points into `buf`, an `IoBuf`, whose bytes stay put
        // when it moves.
        let (res, _, buf) = unsafe { Op::new(io, buf) }.await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Flushes data and metadata to the storage device (`fsync`).
    ///
    /// Cancel safe: if the future is dropped, the flush may or may not have
    /// happened, and calling again does it.
    pub async fn sync_all(&self) -> io::Result<()> {
        self.fsync(false).await
    }

    /// Flushes data, and only the metadata needed to read it back
    /// (`fdatasync`).
    ///
    /// Cancel safe, like [`sync_all`](Self::sync_all).
    pub async fn sync_data(&self) -> io::Result<()> {
        self.fsync(true).await
    }

    /// Reserves disk space for the bytes `offset..offset + len`
    /// (`fallocate`), so that writing there later cannot fail for lack of
    /// space. A shorter file grows to `offset + len`, and the new bytes read
    /// as zeros; data already in the file is not touched.
    ///
    /// A `len` of 0 does nothing (`fallocate` itself would refuse it). A
    /// range that ends above `i64::MAX` is refused with
    /// [`OffsetOutOfRange`](crate::error::Code::OffsetOutOfRange).
    ///
    /// Fails with `Unsupported` on a filesystem without `fallocate` (some
    /// network filesystems).
    ///
    /// # Platform
    ///
    /// On macOS (`F_PREALLOCATE`) only the part past the end of the file is
    /// reserved: holes inside a sparse file stay holes, and writing into
    /// them can still fail for lack of space. The file is grown in a
    /// separate step, so a write past `offset + len` that runs at the same
    /// time can be cut off: do not race the two there.
    ///
    /// Cancel safe: if the future is dropped, the space may or may not have
    /// been reserved, and calling again reserves it.
    pub async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        if offset
            .checked_add(len)
            .map_or(true, |end| end > i64::MAX as u64)
        {
            return Err(raise(
                Code::OffsetOutOfRange,
                format!(
                    "the {len} bytes at file offset {offset} end above {}, the largest offset \
                     Linux has",
                    i64::MAX
                ),
                "check the arithmetic that computed the offset and the length for a \
                 wrap-around",
            ));
        }
        let io = Io::Fallocate {
            fd: self.fd,
            offset,
            len,
        };
        // SAFETY: the operation has no pointers.
        let (res, _, ()) = unsafe { Op::new(io, ()) }.await;
        match res {
            0.. => Ok(()),
            // The kind is set here, as older standard libraries leave
            // EOPNOTSUPP uncategorised; the OS error is kept inside.
            res if -res == crate::sys::errno::EOPNOTSUPP || -res == crate::sys::errno::ENOTSUP => {
                Err(io::Error::new(io::ErrorKind::Unsupported, os_err(res)))
            }
            res => Err(os_err(res)),
        }
    }

    async fn fsync(&self, datasync: bool) -> io::Result<()> {
        let io = Io::Fsync {
            fd: self.fd,
            datasync,
        };
        // SAFETY: the operation has no pointers.
        let (res, _, ()) = unsafe { Op::new(io, ()) }.await;
        if res < 0 {
            Err(os_err(res))
        } else {
            Ok(())
        }
    }

    /// Size, type and modification time of the file.
    ///
    /// Cancel safe: it changes nothing.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        // An empty path with AT_EMPTY_PATH means "the descriptor itself".
        statx(
            self.fd,
            KernelBox::path(CString::default()),
            crate::sys::AT_EMPTY_PATH,
        )
        .await
    }

    /// The size of the file in bytes, read with a plain `fstat` (a system
    /// call that does not wait for the disk). [`metadata`](Self::metadata)
    /// returns the same number, together with everything else, through the
    /// ring.
    pub fn len(&self) -> io::Result<u64> {
        // SAFETY: borrows the descriptor for the duration of the call; the
        // `ManuallyDrop` keeps the temporary `std::fs::File` from closing it.
        let file = mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(self.fd) });
        Ok(file.metadata()?.len())
    }

    /// `true` if the file is empty.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Closes the file and reports the result. Dropping a `File` closes it
    /// too, but cannot report an error.
    ///
    /// Cancel safe for the descriptor, not for the result: the close is never
    /// cancelled. If the future is dropped, before or after it was first
    /// polled, the descriptor is still closed exactly once, on both drivers;
    /// only the error, if any, is lost. Await it to the end to see the error.
    pub async fn close(self) -> io::Result<()> {
        let fd = self.fd;
        mem::forget(self);
        close(fd).await
    }
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let fd = self.fd;
        // Through the current worker's ring, or directly if there is none.
        let closed = runtime::try_with_worker(|w| match w.driver.try_borrow_mut() {
            Ok(mut driver) => driver.close_fd(fd),
            Err(_) => w.defer_close(fd),
        })
        .is_some();
        if !closed {
            // SAFETY: the descriptor is ours and is closed exactly once.
            unsafe { libc::close(fd) };
        }
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File").field("fd", &self.fd).finish()
    }
}

async fn close(fd: RawFd) -> io::Result<()> {
    // ThreadSanitizer keeps track of descriptors, but cannot see a close that
    // goes through the ring (see `Driver::close_fd`). The epoll driver's
    // close is a close(2) it sees.
    #[cfg(giong_tsan)]
    if runtime::driver_kind() == crate::DriverKind::IoUring {
        // Queued operations on `fd` go to the kernel first, or they could
        // act on whatever gets this number next.
        runtime::with_worker(|w| w.driver.borrow_mut().submit_queued());
        // SAFETY: the descriptor is ours and is closed exactly once.
        return if unsafe { libc::close(fd) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        };
    }
    // SAFETY: the operation has no pointers.
    let (res, _, ()) = unsafe { Op::new(Io::Close { fd }, ()) }.await;
    if res < 0 {
        Err(os_err(res))
    } else {
        Ok(())
    }
}

/// A directory that paths are opened beneath, and never outside of.
///
/// A path from a request, an upload or a configuration file can hold `..`,
/// start with `/`, or reach a symbolic link someone placed in the
/// directory: joined to the directory and opened, it can lead anywhere on
/// the machine. A `Root` opens paths relative to the directory it holds
/// open and refuses, with `GIONG-E0205`, any path that is absolute, has a
/// `..` component, or goes through a symbolic link (wherever the link
/// points). The kernel checks during the open itself, so nothing can swap
/// a directory for a link between a check and the open, as it can with a
/// check made beforehand:
///
/// - On Linux, `openat2(2)` with `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS`
///   and `RESOLVE_NO_MAGICLINKS`, as an io_uring operation on the io_uring
///   driver. Where a seccomp filter hides `openat2` from the epoll driver's
///   threads, the path is opened one component at a time with
///   `O_NOFOLLOW`, which refuses the same paths.
/// - On macOS, `openat(2)` with `O_NOFOLLOW_ANY` (macOS 11).
/// - On Windows, each component is opened without following reparse
///   points, and symbolic links and junctions are refused.
///
/// ```no_run
/// use giong::fs::Root;
///
/// # async fn demo(request_path: &str) -> std::io::Result<()> {
/// let public = Root::new("public").await?;
/// // "css/site.css" opens; "../.env", "/etc/passwd" and a link to either
/// // are refused.
/// let file = public.open(request_path).await?;
/// # Ok(())
/// # }
/// ```
///
/// The directory itself may be reached through a link (`Root::new`
/// follows links like any open): only what is beneath it is checked.
/// Like a [`File`], a `Root` belongs to the worker that opened it; opening
/// one in each worker costs one open.
pub struct Root {
    /// The directory, opened with `O_PATH` on Linux (no read access needed).
    /// Each open beneath it holds a reference until the open is done, so a
    /// root dropped meanwhile does not close it: the kernel looks the
    /// descriptor up when the open runs, and the number could by then name
    /// another directory.
    dir: Rc<File>,
    path: PathBuf,
}

/// How a root's directory is opened.
#[cfg(target_os = "linux")]
const ROOT_FLAGS: i32 = libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC;
#[cfg(not(target_os = "linux"))]
const ROOT_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;

/// How many times an open beneath a root is tried when `openat2` answers
/// `EAGAIN`: it may when a rename elsewhere raced with the lookup.
const ROOT_ATTEMPTS: u32 = 16;

impl Root {
    /// Opens the directory `dir` as a root.
    ///
    /// Cancel safe: if the future is dropped, the descriptor the open may
    /// still produce is closed, and nothing else changes.
    pub async fn new<P: AsRef<Path>>(dir: P) -> io::Result<Self> {
        let dir = dir.as_ref();
        let path = c_path(dir)?;
        let io = Io::OpenAt {
            path: path.c_ptr(),
            flags: ROOT_FLAGS,
            mode: 0,
        };
        let fd = path_op(path, io).await?;
        Ok(Self {
            dir: Rc::new(File::from_fd(fd)),
            path: dir.to_path_buf(),
        })
    }

    /// Opens the file at `path` beneath the root for reading.
    ///
    /// Cancel safe: if the future is dropped, the descriptor the open may
    /// still produce is closed, and nothing else changes.
    pub async fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        OpenOptions::new().read(true).open_in(self, path).await
    }

    /// Opens the file at `path` beneath the root for writing, creating it
    /// if needed and emptying it if it exists.
    ///
    /// Not cancel safe: if the future is dropped, the file may have been
    /// created or emptied (the descriptor is closed either way).
    pub async fn create<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open_in(self, path)
            .await
    }

    /// Reads the whole file at `path` beneath the root, as [`read`] does.
    ///
    /// Cancel safe for a regular file: if the future is dropped, nothing
    /// changes and the file is closed.
    pub async fn read<P: AsRef<Path>>(&self, path: P) -> io::Result<Vec<u8>> {
        read_all(self.open(path).await?).await
    }

    /// Size, type and modification time of what is at `path` beneath the
    /// root. A symbolic link there is refused, as everywhere under a root.
    ///
    /// Cancel safe: it changes nothing.
    pub async fn metadata<P: AsRef<Path>>(&self, path: P) -> io::Result<Metadata> {
        let path = path.as_ref();
        #[cfg(target_os = "linux")]
        {
            // Linux has no stat with `RESOLVE_BENEATH`: an `O_PATH` open
            // beneath the root, then a stat of what it opened.
            let fd = self
                .open_beneath(path, libc::O_PATH | libc::O_CLOEXEC, 0)
                .await?;
            File::from_fd(fd).metadata().await
        }
        #[cfg(not(target_os = "linux"))]
        {
            self.check(path)?;
            let meta = statx(self.dir.fd, c_path(path)?, sys::AT_SYMLINK_NOFOLLOW_ANY)
                .await
                .map_err(|e| self.refused(path, e))?;
            // `AT_SYMLINK_NOFOLLOW_ANY` refuses a link on the way but, like
            // `AT_SYMLINK_NOFOLLOW`, describes one at the end of the path.
            if meta.is_symlink() {
                return Err(self.outside(path, "goes through a symbolic link"));
            }
            Ok(meta)
        }
    }

    /// Opens the directory at `path` beneath this root as a root of its
    /// own, for paths beneath it.
    ///
    /// Cancel safe: if the future is dropped, the descriptor the open may
    /// still produce is closed, and nothing else changes.
    pub async fn open_root<P: AsRef<Path>>(&self, path: P) -> io::Result<Root> {
        let path = path.as_ref();
        let fd = self.open_beneath(path, ROOT_FLAGS, 0).await?;
        Ok(Root {
            dir: Rc::new(File::from_fd(fd)),
            path: self.path.join(path),
        })
    }

    /// The directory's path, as given to [`new`](Self::new) (joined with
    /// the path given to [`open_root`](Self::open_root)).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Refuses, before asking the kernel, an absolute path and one with a
    /// `..` component.
    fn check(&self, path: &Path) -> io::Result<()> {
        use std::path::Component;
        for part in path.components() {
            match part {
                Component::RootDir => return Err(self.outside(path, "is absolute")),
                Component::ParentDir => return Err(self.outside(path, "has a .. component")),
                _ => {}
            }
        }
        Ok(())
    }

    /// The descriptor of what is at `path` beneath the root, opened with
    /// `flags` and `mode`.
    async fn open_beneath(&self, path: &Path, flags: i32, mode: u32) -> io::Result<RawFd> {
        self.check(path)?;
        let mut name = c_path(path)?;
        let mut how = KernelBox::new(sys::open_how(flags, mode));
        let mut attempts = 1;
        loop {
            let io = Io::OpenBeneath {
                dir: self.dir.fd,
                path: name.c_ptr(),
                how: how.as_ptr(),
            };
            // SAFETY: the operation's pointers are to the path and to `how`,
            // both in `KernelBox`es. The directory goes with them, so that it
            // stays open until the operation is done even if the root and
            // this future are dropped.
            let (res, _, back) = unsafe { Op::new(io, (name, how, Rc::clone(&self.dir))) }.await;
            (name, how, _) = back;
            if res >= 0 {
                return Ok(res);
            }
            if res == -libc::EAGAIN && attempts < ROOT_ATTEMPTS {
                attempts += 1;
                continue;
            }
            return Err(self.refused(path, os_err(res)));
        }
    }

    /// `e` as `GIONG-E0205` when the kernel refused the path as leading out
    /// of the root.
    fn refused(&self, path: &Path, e: io::Error) -> io::Error {
        match e.raw_os_error() {
            Some(libc::ELOOP) => self.outside(path, "goes through a symbolic link"),
            Some(libc::EXDEV) => self.outside(path, "leads outside the directory"),
            _ => e,
        }
    }

    #[cold]
    fn outside(&self, path: &Path, why: &str) -> io::Error {
        raise(
            Code::OutsideRoot,
            format!(
                "the path {path:?} {why}, so it is not opened beneath the root {:?}",
                self.path
            ),
            "give a path relative to the root, without .. and without symbolic links on the \
             way; to open files in a directory a link leads to, open a Root on that directory",
        )
    }
}

impl std::fmt::Debug for Root {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Root")
            .field("path", &self.path)
            .field("fd", &self.dir.fd)
            .finish()
    }
}

/// What [`metadata`] and [`File::metadata`] report about a file.
#[derive(Clone, Debug)]
pub struct Metadata {
    len: u64,
    mode: u32,
    modified: SystemTime,
}

impl Metadata {
    /// Size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` for an empty file.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `true` for a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type().is_file()
    }

    /// `true` for a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type().is_dir()
    }

    /// `true` for a symbolic link (only reported by [`symlink_metadata`]).
    pub fn is_symlink(&self) -> bool {
        self.file_type().is_symlink()
    }

    /// The type of the file.
    pub fn file_type(&self) -> FileType {
        FileType(self.mode & sys::mode::S_IFMT)
    }

    /// File type and permission bits, as in `st_mode`.
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// When the contents were last modified.
    pub fn modified(&self) -> SystemTime {
        self.modified
    }
}

async fn statx(dir: RawFd, path: KernelBox<CString>, flags: i32) -> io::Result<Metadata> {
    // SAFETY: all-zero is a valid stat buffer (plain integers).
    let out: KernelBox<sys::StatBuf> = KernelBox::new(unsafe { mem::zeroed() });
    let mask = sys::STAT_MASK;
    let io = Io::Statx {
        dir,
        path: path.c_ptr(),
        flags,
        mask,
        out: out.as_ptr(),
    };
    // SAFETY: the path and the output are in `KernelBox`es.
    let (res, _, (_path, out)) = unsafe { Op::new(io, (path, out)) }.await;
    if res < 0 {
        return Err(os_err(res));
    }
    let (len, mode, mtime_sec, mtime_nsec) = sys::stat_fields(out.get());
    let since_epoch = Duration::new(mtime_sec.unsigned_abs(), mtime_nsec);
    let modified = if mtime_sec >= 0 {
        SystemTime::UNIX_EPOCH + since_epoch
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(mtime_sec.unsigned_abs())
            + Duration::from_nanos(mtime_nsec.into())
    };
    Ok(Metadata {
        len,
        mode,
        modified,
    })
}

/// Size, type and modification time of the file at `path`, following
/// symbolic links.
///
/// Cancel safe: it changes nothing.
pub async fn metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    statx(libc::AT_FDCWD, c_path(path.as_ref())?, 0).await
}

/// Like [`metadata`], but describes a symbolic link itself instead of what it
/// points at.
///
/// Cancel safe: it changes nothing.
pub async fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    statx(
        libc::AT_FDCWD,
        c_path(path.as_ref())?,
        libc::AT_SYMLINK_NOFOLLOW,
    )
    .await
}

/// Reads a whole file.
///
/// Cancel safe for a regular file: if the future is dropped, nothing changes
/// and the file is closed.
pub async fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    read_all(File::open(path).await?).await
}

/// The whole of `file`, from its start.
async fn read_all(file: File) -> io::Result<Vec<u8>> {
    // The size is only a hint: files in /proc report 0, and a file may grow
    // while it is being read.
    // Read with a plain `fstat` (see `File::len`): one system call instead
    // of a trip through the ring for a number that is only a hint.
    let hint = file.len().unwrap_or(0) as usize;
    // Every allocation is fallible: a file that claims more than memory
    // holds (a sparse disk image, a core dump) gives an error, not an
    // abort of the whole process.
    let mut data = Vec::new();
    reserve(&mut data, hint.saturating_add(1))?;
    let mut chunk = Vec::new();
    loop {
        if data.len() == data.capacity() {
            let more = data.capacity().clamp(4096, 1 << 20);
            reserve(&mut data, more)?;
        }
        // Read straight into the result while it is empty (the common case
        // of one read for the whole file), into a scratch buffer afterwards.
        if data.is_empty() {
            let (res, buf) = file.read_at(data, 0).await;
            data = buf;
            if res? == 0 {
                return Ok(data);
            }
        } else {
            chunk.clear();
            let room = (data.capacity() - data.len()).min(1 << 20);
            reserve(&mut chunk, room)?;
            let (res, buf) = file.read_at(chunk, data.len() as u64).await;
            chunk = buf;
            if res? == 0 {
                return Ok(data);
            }
            reserve(&mut data, chunk.len())?;
            data.extend_from_slice(&chunk);
        }
    }
}

/// `Vec::try_reserve`, failing with [`Code::FileTooLarge`].
fn reserve(v: &mut Vec<u8>, additional: usize) -> io::Result<()> {
    v.try_reserve(additional)
        .map_err(|_| file_too_large(v.len(), additional))
}

#[cold]
fn file_too_large(have: usize, more: usize) -> io::Error {
    raise(
        Code::FileTooLarge,
        format!(
            "fs::read could not allocate {} more bytes (after {have}) to hold the whole file",
            more
        ),
        "read the file in pieces with File::read_at, or check its length (File::len) before \
         reading it whole; a sparse file can claim far more than it holds",
    )
}

/// Writes `data` to `path`, creating the file or replacing its contents.
///
/// The data is not synced to the storage device; open the file yourself and
/// call [`File::sync_all`] where that matters.
///
/// Not cancel safe: if the future is dropped, the file may have been created or
/// emptied and only part of `data` written.
pub async fn write<P: AsRef<Path>, B: IoBuf>(path: P, data: B) -> io::Result<()> {
    let file = File::create(path).await?;
    file.write_all_at(data, 0).await.0?;
    file.close().await
}

/// Removes a file.
///
/// Not cancel safe: if the future is dropped, the file may have been removed,
/// and calling again then fails with `NotFound`.
pub async fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let io = Io::UnlinkAt {
        path: path.c_ptr(),
        flags: 0,
    };
    path_op(path, io).await.map(drop)
}

/// Removes an empty directory.
///
/// Not cancel safe, like [`remove_file`].
pub async fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let io = Io::UnlinkAt {
        path: path.c_ptr(),
        flags: libc::AT_REMOVEDIR,
    };
    path_op(path, io).await.map(drop)
}

/// Creates a directory (permissions `0o777` before the umask).
///
/// Not cancel safe: if the future is dropped, the directory may have been
/// created, and calling again then fails with `AlreadyExists`.
pub async fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let io = Io::MkDirAt {
        path: path.c_ptr(),
        mode: 0o777,
    };
    path_op(path, io).await.map(drop)
}

/// Renames a file or directory, replacing `to` if it exists.
///
/// Not cancel safe: if the future is dropped, the rename may have happened, and
/// calling again then fails with `NotFound`.
pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    let paths = (c_path(from.as_ref())?, c_path(to.as_ref())?);
    let io = Io::RenameAt {
        from: paths.0.c_ptr(),
        to: paths.1.c_ptr(),
    };
    // SAFETY: both paths are in `KernelBox`es.
    let (res, _, _paths) = unsafe { Op::new(io, paths) }.await;
    if res < 0 {
        Err(os_err(res))
    } else {
        Ok(())
    }
}

/// What kind of file an entry is: a regular file, a directory, a symbolic
/// link, or something else (a socket, a pipe, a device).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType(u32);

impl FileType {
    /// `true` for a regular file.
    pub fn is_file(&self) -> bool {
        self.0 == sys::mode::S_IFREG
    }

    /// `true` for a directory.
    pub fn is_dir(&self) -> bool {
        self.0 == sys::mode::S_IFDIR
    }

    /// `true` for a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.0 == sys::mode::S_IFLNK
    }

    /// The type as the `S_IFMT` bits of `st_mode` (`S_IFREG`, `S_IFDIR`, ...).
    pub fn mode(&self) -> u32 {
        self.0
    }

    /// The type a directory entry reports, if the filesystem reports one.
    fn from_dirent(d_type: u8) -> Option<Self> {
        let mode = match d_type {
            libc::DT_REG => sys::mode::S_IFREG,
            libc::DT_DIR => sys::mode::S_IFDIR,
            libc::DT_LNK => sys::mode::S_IFLNK,
            libc::DT_FIFO => sys::mode::S_IFIFO,
            libc::DT_SOCK => sys::mode::S_IFSOCK,
            libc::DT_CHR => sys::mode::S_IFCHR,
            libc::DT_BLK => sys::mode::S_IFBLK,
            _ => return None,
        };
        Some(Self(mode))
    }
}

/// How many bytes of entries one `getdents64` call may return. A call holds
/// up the worker for a time that grows with the entries it returns: at 8 KiB,
/// some tens of microseconds on a local disk, four times less than at 32 KiB,
/// for a whole listing a few percent slower.
const DIR_BATCH: usize = 8 * 1024;

/// A batch read on the worker that takes this long (a cold disk, a slow
/// filesystem this list took for local) sends the batches after it to the
/// blocking lane.
#[cfg(target_os = "linux")]
const SLOW_DIR_BATCH: Duration = Duration::from_millis(1);

/// Lists the entries of a directory, without `.` and `..`, in the order the
/// filesystem keeps them.
///
/// ```no_run
/// # async fn demo() -> std::io::Result<()> {
/// let mut dir = giong::fs::read_dir("/etc").await?;
/// while let Some(entry) = dir.next_entry().await? {
///     if entry.file_type().await?.is_file() {
///         println!("{}", entry.path().display());
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// The directory is opened through the ring, but io_uring has no operation
/// that reads a directory. So each batch of entries (8 KiB of them, about a
/// hundred names) is read with a direct `getdents64` system call on the
/// worker thread, and the task yields between batches. On a local
/// filesystem with the directory cached, a batch takes some tens of
/// microseconds.
///
/// On Linux, a directory on a network or FUSE filesystem (NFS, SMB, 9p,
/// Ceph, FUSE, or any filesystem giong does not know as local) is read on
/// the blocking lane ([`spawn_blocking`](crate::spawn_blocking)) instead,
/// where a batch that takes a round trip, or hangs with its server, holds up
/// one blocking thread and not the worker. So is the rest of a directory
/// once one batch took a millisecond or more on the worker (a cold disk).
///
/// Cancel safe: if the future is dropped, nothing changes and the directory is
/// closed.
pub async fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let path = path.as_ref();
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
        .await?;
    #[cfg(target_os = "linux")]
    let off_worker = if sys::on_local_fs(dir.fd) {
        None
    } else {
        Some(OffWorker::new(dir.fd)?)
    };
    Ok(ReadDir {
        dir,
        path: Arc::from(path),
        buf: Vec::new(),
        pos: 0,
        yield_first: false,
        done: false,
        state: sys::DirState::default(),
        #[cfg(target_os = "linux")]
        off_worker,
    })
}

/// A directory whose batches are read on the blocking lane.
#[cfg(target_os = "linux")]
struct OffWorker {
    /// A duplicate of the directory's descriptor, which shares its position:
    /// a batch still running when the `ReadDir` is dropped reads through it,
    /// never through a closed (or reused) number.
    fd: Arc<std::os::fd::OwnedFd>,
    /// The batch being read: kept across a dropped `next_entry`, so that its
    /// entries are not lost.
    pending: Option<crate::lane::BlockingHandle<io::Result<Vec<u8>>>>,
}

#[cfg(target_os = "linux")]
impl OffWorker {
    fn new(fd: RawFd) -> io::Result<Self> {
        // SAFETY: `fd` is open for the duration of the call.
        let fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }.try_clone_to_owned()?;
        Ok(Self {
            fd: Arc::new(fd),
            pending: None,
        })
    }
}

/// The entries of a directory; see [`read_dir`].
pub struct ReadDir {
    dir: File,
    path: Arc<Path>,
    /// The last batch of `linux_dirent64` records.
    buf: Vec<u8>,
    /// Where the next record starts in `buf`.
    pos: usize,
    /// Yield before reading the next batch.
    yield_first: bool,
    /// The last batch was empty: the end of the directory.
    done: bool,
    /// What the system keeps between batches (a directory stream on macOS).
    state: sys::DirState,
    /// Set when the batches are read on the blocking lane.
    #[cfg(target_os = "linux")]
    off_worker: Option<OffWorker>,
}

impl ReadDir {
    /// The next entry, or `None` when there are no more.
    ///
    /// Cancel safe: if the future is dropped, no entry is lost.
    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        loop {
            while let Some(entry) = self.parse() {
                if let Some(entry) = entry {
                    return Ok(Some(entry));
                }
            }
            if self.done {
                return Ok(None);
            }
            if mem::take(&mut self.yield_first) {
                // Between batches, let other tasks and the ring run. If this
                // future is dropped here, the next call reads on without
                // yielding again, so even a zero timeout makes progress.
                crate::yield_now().await;
            }
            #[cfg(target_os = "linux")]
            if self.off_worker.is_some() {
                self.read_batch_off_worker().await?;
                continue;
            }
            self.read_batch()?;
        }
    }

    /// Reads the next batch on the blocking lane, and waits for it.
    #[cfg(target_os = "linux")]
    async fn read_batch_off_worker(&mut self) -> io::Result<()> {
        let off = self.off_worker.as_mut().expect("reading off the worker");
        let pending = off.pending.get_or_insert_with(|| {
            let fd = off.fd.clone();
            let mut buf = mem::take(&mut self.buf);
            crate::spawn_blocking(move || {
                buf.clear();
                buf.reserve(DIR_BATCH);
                let mut state = sys::DirState::default();
                sys::read_dir_batch(&mut state, fd.as_raw_fd(), &mut buf).map(|_| buf)
            })
        });
        let res = pending.await;
        off.pending = None;
        let buf = res.map_err(|e| {
            io::Error::other(format!(
                "a batch of entries of {} was not read: {e}",
                self.path.display()
            ))
        })??;
        self.done = buf.is_empty();
        self.buf = buf;
        self.pos = 0;
        Ok(())
    }

    /// Reads the next record of the batch and moves past it. `None` at the
    /// end of the batch; `Some(None)` for `.` and `..`.
    fn parse(&mut self) -> Option<Option<DirEntry>> {
        // struct linux_dirent64 { u64 d_ino; i64 d_off; u16 d_reclen;
        //                         u8 d_type; char d_name[]; }
        let record = self.buf.get(self.pos..)?;
        let field = |at: usize, len: usize| record.get(at..at + len);
        let ino = u64::from_ne_bytes(field(0, 8)?.try_into().ok()?);
        let reclen = u16::from_ne_bytes(field(16, 2)?.try_into().ok()?) as usize;
        let d_type = *record.get(18)?;
        let name = record.get(19..reclen)?;
        let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
        let entry = (name != b"." && name != b"..").then(|| DirEntry {
            dir: self.path.clone(),
            name: OsString::from_vec(name.to_vec()),
            ino,
            file_type: FileType::from_dirent(d_type),
        });
        self.pos += reclen;
        Some(entry)
    }

    fn read_batch(&mut self) -> io::Result<()> {
        self.buf.clear();
        self.buf.reserve(DIR_BATCH);
        #[cfg(target_os = "linux")]
        let start = std::time::Instant::now();
        let n = sys::read_dir_batch(&mut self.state, self.dir.fd, &mut self.buf)?;
        self.pos = 0;
        self.yield_first = true;
        self.done = n == 0;
        #[cfg(target_os = "linux")]
        if !self.done && start.elapsed() >= SLOW_DIR_BATCH {
            // A failed `dup` only keeps the batches on the worker.
            self.off_worker = OffWorker::new(self.dir.fd).ok();
        }
        Ok(())
    }
}

impl std::fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadDir").field("path", &self.path).finish()
    }
}

/// One entry of a directory, from [`ReadDir::next_entry`].
#[derive(Clone, Debug)]
pub struct DirEntry {
    dir: Arc<Path>,
    name: OsString,
    ino: u64,
    /// `None` if the filesystem did not say (`DT_UNKNOWN`).
    file_type: Option<FileType>,
}

impl DirEntry {
    /// The entry's name, without the directory.
    pub fn file_name(&self) -> &OsStr {
        &self.name
    }

    /// The directory given to [`read_dir`] joined with the entry's name.
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    /// The inode number.
    pub fn ino(&self) -> u64 {
        self.ino
    }

    /// The type of the entry itself (a symbolic link is not followed). Most
    /// filesystems report it with the name, so no system call is needed; for
    /// the others it is looked up through the ring.
    ///
    /// Cancel safe: it changes nothing.
    pub async fn file_type(&self) -> io::Result<FileType> {
        match self.file_type {
            Some(file_type) => Ok(file_type),
            None => Ok(self.metadata().await?.file_type()),
        }
    }

    /// Metadata of the entry itself, like [`symlink_metadata`] on
    /// [`path`](Self::path).
    ///
    /// Cancel safe: it changes nothing.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        symlink_metadata(self.path()).await
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::future::Future;

    fn names_in(dir: &Path, off_worker: bool, cancel_every: usize) -> Vec<OsString> {
        let dir = dir.to_owned();
        crate::block_on(async move {
            let mut rd = read_dir(&dir).await.unwrap();
            if off_worker {
                rd.off_worker = Some(OffWorker::new(rd.dir.fd).unwrap());
            }
            let mut names = Vec::new();
            let mut polls = 0usize;
            loop {
                polls += 1;
                // Drops some calls after one poll, mid-batch on the blocking
                // lane: no entry may be lost.
                if cancel_every != 0 && polls % cancel_every == 0 {
                    let mut next = std::pin::pin!(rd.next_entry());
                    struct Noop;
                    impl std::task::Wake for Noop {
                        fn wake(self: Arc<Self>) {}
                    }
                    let waker = std::task::Waker::from(Arc::new(Noop));
                    let mut cx = std::task::Context::from_waker(&waker);
                    if let std::task::Poll::Ready(entry) = next.as_mut().poll(&mut cx) {
                        match entry.unwrap() {
                            Some(e) => names.push(e.file_name().to_owned()),
                            None => break,
                        }
                    }
                    continue;
                }
                match rd.next_entry().await.unwrap() {
                    Some(e) => names.push(e.file_name().to_owned()),
                    None => break,
                }
            }
            names.sort();
            names
        })
    }

    /// A directory read on the blocking lane lists the same entries as one
    /// read on the worker, also when calls are dropped while a batch runs.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no fstatfs or getdents64")]
    fn a_directory_read_off_the_worker_lists_every_entry_once() {
        let dir = std::env::temp_dir().join(format!("giong-read-dir-off-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        for i in 0..1500 {
            std::fs::write(dir.join(format!("entry-with-a-longish-name-{i:05}")), b"").unwrap();
        }
        let on = names_in(&dir, false, 0);
        assert_eq!(on.len(), 1500);
        assert_eq!(names_in(&dir, true, 0), on);
        assert_eq!(names_in(&dir, true, 3), on);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn local_and_network_filesystems() {
        assert!(sys::is_local_fs_magic(0xEF53)); // ext4
        assert!(sys::is_local_fs_magic(0x0102_1994)); // tmpfs
        assert!(sys::is_local_fs_magic(0x794C_7630)); // overlayfs
        assert!(!sys::is_local_fs_magic(0x6969)); // NFS
        assert!(!sys::is_local_fs_magic(0xFF53_4D42)); // CIFS
        assert!(!sys::is_local_fs_magic(0xFE53_4D42)); // SMB2
        assert!(!sys::is_local_fs_magic(0x6573_5546)); // FUSE
        assert!(!sys::is_local_fs_magic(0x0102_1997)); // 9p
        assert!(!sys::is_local_fs_magic(0x00C3_6400)); // Ceph
        assert!(!sys::is_local_fs_magic(0)); // unknown
    }
}
