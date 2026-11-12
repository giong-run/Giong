//! Files on io_uring.
//!
//! Unlike sockets, regular files are never "ready" in the `epoll` sense, which
//! is why other runtimes send file I/O to a pool of blocking threads. With
//! io_uring it goes through the same ring as everything else: opens, reads,
//! writes and syncs are submitted and completed in the same batches as
//! network I/O, and the worker thread never blocks on the disk.
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

use std::ffi::CString;
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use io_uring::{opcode, types};

use crate::buf::{IoBuf, IoBufMut};
use crate::error::{raise, Code};
use crate::op::Op;
use crate::runtime;

fn os_err(res: i32) -> io::Error {
    io::Error::from_raw_os_error(-res)
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        raise(
            Code::InvalidPath,
            format!("the path {path:?} contains a NUL byte"),
            "remove the NUL byte; Linux paths cannot contain one.",
        )
    })
}

fn cwd() -> types::Fd {
    types::Fd(libc::AT_FDCWD)
}

/// Runs an operation whose only resource is one path.
async fn path_op(path: CString, sqe: io_uring::squeue::Entry) -> io::Result<i32> {
    let (res, _, _path) = Op::new(sqe, path).await;
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

    /// Extra `open(2)` flags, for example `libc::O_DIRECT`.
    pub fn custom_flags(mut self, flags: i32) -> Self {
        self.custom_flags = flags;
        self
    }

    fn flags(&self) -> io::Result<i32> {
        let invalid = || io::Error::from_raw_os_error(libc::EINVAL);
        let access = match (self.read, self.write, self.append) {
            (true, false, false) => libc::O_RDONLY,
            (false, true, false) => libc::O_WRONLY,
            (true, true, false) => libc::O_RDWR,
            (false, _, true) => libc::O_WRONLY | libc::O_APPEND,
            (true, _, true) => libc::O_RDWR | libc::O_APPEND,
            (false, false, false) => return Err(invalid()),
        };
        let writable = self.write || self.append;
        if !writable && (self.truncate || self.create || self.create_new) {
            return Err(invalid());
        }
        if self.append && self.truncate && !self.create_new {
            return Err(invalid());
        }
        let creation = match (self.create, self.truncate, self.create_new) {
            (_, _, true) => libc::O_CREAT | libc::O_EXCL,
            (false, false, false) => 0,
            (true, false, false) => libc::O_CREAT,
            (false, true, false) => libc::O_TRUNC,
            (true, true, false) => libc::O_CREAT | libc::O_TRUNC,
        };
        Ok(access | creation | libc::O_CLOEXEC | self.custom_flags)
    }

    /// Opens `path` with these options.
    pub async fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        let flags = self.flags()?;
        let path = c_path(path.as_ref())?;
        let sqe = opcode::OpenAt::new(cwd(), path.as_ptr())
            .flags(flags)
            .mode(self.mode)
            .build();
        let fd = path_op(path, sqe).await?;
        Ok(File::from_fd(fd))
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
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
    pub async fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        OpenOptions::new().read(true).open(path).await
    }

    /// Opens a file for writing, creating it if needed and emptying it if it
    /// exists.
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
    pub async fn read_at<B: IoBufMut>(&self, mut buf: B, offset: u64) -> (io::Result<usize>, B) {
        let len = buf.capacity().min(u32::MAX as usize) as u32;
        let sqe = opcode::Read::new(types::Fd(self.fd), buf.as_mut_ptr(), len)
            .offset(offset)
            .build();
        let (res, _, mut buf) = Op::new(sqe, buf).await;
        if res < 0 {
            return (Err(os_err(res)), buf);
        }
        // SAFETY: the kernel wrote `res` bytes at the start of the buffer.
        unsafe { buf.set_init(res as usize) };
        (Ok(res as usize), buf)
    }

    /// Writes some bytes of `buf` at `offset`, returning how many were
    /// written.
    pub async fn write_at<B: IoBuf>(&self, buf: B, offset: u64) -> (io::Result<usize>, B) {
        self.write_from(buf, 0, offset).await
    }

    /// Writes all of `buf` at `offset`. The buffer is handed back so it can be
    /// reused.
    pub async fn write_all_at<B: IoBuf>(&self, mut buf: B, offset: u64) -> (io::Result<()>, B) {
        let mut written = 0;
        while written < buf.len() {
            let (res, b) = self.write_from(buf, written, offset + written as u64).await;
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
        let len = (buf.len() - skip).min(u32::MAX as usize) as u32;
        // SAFETY: `skip <= len`, so the pointer stays within the buffer.
        let ptr = unsafe { buf.as_ptr().add(skip) };
        let sqe = opcode::Write::new(types::Fd(self.fd), ptr, len)
            .offset(offset)
            .build();
        let (res, _, buf) = Op::new(sqe, buf).await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Flushes data and metadata to the storage device (`fsync`).
    pub async fn sync_all(&self) -> io::Result<()> {
        self.fsync(types::FsyncFlags::empty()).await
    }

    /// Flushes data, and only the metadata needed to read it back
    /// (`fdatasync`).
    pub async fn sync_data(&self) -> io::Result<()> {
        self.fsync(types::FsyncFlags::DATASYNC).await
    }

    async fn fsync(&self, flags: types::FsyncFlags) -> io::Result<()> {
        let sqe = opcode::Fsync::new(types::Fd(self.fd)).flags(flags).build();
        let (res, _, ()) = Op::new(sqe, ()).await;
        if res < 0 {
            Err(os_err(res))
        } else {
            Ok(())
        }
    }

    /// Size, type and modification time of the file.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        // An empty path with AT_EMPTY_PATH means "the descriptor itself".
        statx(types::Fd(self.fd), CString::default(), libc::AT_EMPTY_PATH).await
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
            Ok(mut driver) => {
                driver.close_fd(fd);
                true
            }
            Err(_) => false,
        })
        .unwrap_or(false);
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

#[cfg(not(giong_tsan))]
async fn close(fd: RawFd) -> io::Result<()> {
    let sqe = opcode::Close::new(types::Fd(fd)).build();
    let (res, _, ()) = Op::new(sqe, ()).await;
    if res < 0 {
        Err(os_err(res))
    } else {
        Ok(())
    }
}

/// ThreadSanitizer keeps track of descriptors, but cannot see a close that
/// goes through the ring (see `Driver::close_fd`).
#[cfg(giong_tsan)]
async fn close(fd: RawFd) -> io::Result<()> {
    // SAFETY: the descriptor is ours and is closed exactly once.
    if unsafe { libc::close(fd) } < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
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
        self.mode & libc::S_IFMT == libc::S_IFREG
    }

    /// `true` for a directory.
    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    /// `true` for a symbolic link (only reported by [`symlink_metadata`]).
    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
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

async fn statx(dir: types::Fd, path: CString, flags: i32) -> io::Result<Metadata> {
    // SAFETY: all-zero is a valid `statx` (plain integers).
    let out: Box<libc::statx> = Box::new(unsafe { mem::zeroed() });
    let mask = libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_SIZE | libc::STATX_MTIME;
    let mut resources = (path, out);
    let sqe = opcode::Statx::new(
        dir,
        resources.0.as_ptr(),
        (&mut *resources.1 as *mut libc::statx).cast(),
    )
    .flags(flags)
    .mask(mask)
    .build();
    let (res, _, (_path, out)) = Op::new(sqe, resources).await;
    if res < 0 {
        return Err(os_err(res));
    }
    let since_epoch = Duration::new(out.stx_mtime.tv_sec.unsigned_abs(), out.stx_mtime.tv_nsec);
    let modified = if out.stx_mtime.tv_sec >= 0 {
        SystemTime::UNIX_EPOCH + since_epoch
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_secs(out.stx_mtime.tv_sec.unsigned_abs())
            + Duration::from_nanos(out.stx_mtime.tv_nsec.into())
    };
    Ok(Metadata {
        len: out.stx_size,
        mode: out.stx_mode.into(),
        modified,
    })
}

/// Size, type and modification time of the file at `path`, following
/// symbolic links.
pub async fn metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    statx(cwd(), c_path(path.as_ref())?, 0).await
}

/// Like [`metadata`], but describes a symbolic link itself instead of what it
/// points at.
pub async fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    statx(cwd(), c_path(path.as_ref())?, libc::AT_SYMLINK_NOFOLLOW).await
}

/// Reads a whole file.
pub async fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let file = File::open(path).await?;
    // The size is only a hint: files in /proc report 0, and a file may grow
    // while it is being read.
    let hint = file.metadata().await.map_or(0, |m| m.len()) as usize;
    let mut data = Vec::with_capacity(hint.saturating_add(1));
    let mut chunk = Vec::new();
    loop {
        if data.len() == data.capacity() {
            data.reserve(data.capacity().clamp(4096, 1 << 20));
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
            chunk.reserve((data.capacity() - data.len()).min(1 << 20));
            let (res, buf) = file.read_at(chunk, data.len() as u64).await;
            chunk = buf;
            if res? == 0 {
                return Ok(data);
            }
            data.extend_from_slice(&chunk);
        }
    }
}

/// Writes `data` to `path`, creating the file or replacing its contents.
///
/// The data is not synced to the storage device; open the file yourself and
/// call [`File::sync_all`] where that matters.
pub async fn write<P: AsRef<Path>, B: IoBuf>(path: P, data: B) -> io::Result<()> {
    let file = File::create(path).await?;
    file.write_all_at(data, 0).await.0?;
    file.close().await
}

/// Removes a file.
pub async fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let sqe = opcode::UnlinkAt::new(cwd(), path.as_ptr()).build();
    path_op(path, sqe).await.map(drop)
}

/// Removes an empty directory.
pub async fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let sqe = opcode::UnlinkAt::new(cwd(), path.as_ptr())
        .flags(libc::AT_REMOVEDIR)
        .build();
    path_op(path, sqe).await.map(drop)
}

/// Creates a directory (permissions `0o777` before the umask).
pub async fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let sqe = opcode::MkDirAt::new(cwd(), path.as_ptr())
        .mode(0o777)
        .build();
    path_op(path, sqe).await.map(drop)
}

/// Renames a file or directory, replacing `to` if it exists.
pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    let paths = (c_path(from.as_ref())?, c_path(to.as_ref())?);
    let sqe = opcode::RenameAt::new(cwd(), paths.0.as_ptr(), cwd(), paths.1.as_ptr()).build();
    let (res, _, _paths) = Op::new(sqe, paths).await;
    if res < 0 {
        Err(os_err(res))
    } else {
        Ok(())
    }
}
