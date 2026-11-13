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
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::buf::{IoBuf, IoBufMut};
use crate::driver::Io;
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

/// Runs an operation whose only resource is one path.
async fn path_op(path: CString, io: Io) -> io::Result<i32> {
    let (res, _, _path) = Op::new(io, path).await;
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
        let io = Io::OpenAt {
            path: path.as_ptr(),
            flags,
            mode: self.mode,
        };
        let fd = path_op(path, io).await?;
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
        let io = Io::Read {
            fd: self.fd,
            buf: buf.as_mut_ptr(),
            len,
            offset,
        };
        let (res, _, mut buf) = Op::new(io, buf).await;
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
        let io = Io::Write {
            fd: self.fd,
            buf: ptr,
            len,
            offset,
        };
        let (res, _, buf) = Op::new(io, buf).await;
        if res < 0 {
            (Err(os_err(res)), buf)
        } else {
            (Ok(res as usize), buf)
        }
    }

    /// Flushes data and metadata to the storage device (`fsync`).
    pub async fn sync_all(&self) -> io::Result<()> {
        self.fsync(false).await
    }

    /// Flushes data, and only the metadata needed to read it back
    /// (`fdatasync`).
    pub async fn sync_data(&self) -> io::Result<()> {
        self.fsync(true).await
    }

    /// Reserves disk space for the bytes `offset..offset + len`
    /// (`fallocate`), so that writing there later cannot fail for lack of
    /// space. A shorter file grows to `offset + len`, and the new bytes read
    /// as zeros; data already in the file is not touched.
    ///
    /// Fails with `Unsupported` on a filesystem without `fallocate` (some
    /// network filesystems).
    pub async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        let io = Io::Fallocate {
            fd: self.fd,
            offset,
            len,
        };
        let (res, _, ()) = Op::new(io, ()).await;
        match res {
            0.. => Ok(()),
            // The kind is set here, as older standard libraries leave
            // EOPNOTSUPP uncategorised; the OS error is kept inside.
            res if -res == libc::EOPNOTSUPP => {
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
        let (res, _, ()) = Op::new(io, ()).await;
        if res < 0 {
            Err(os_err(res))
        } else {
            Ok(())
        }
    }

    /// Size, type and modification time of the file.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        // An empty path with AT_EMPTY_PATH means "the descriptor itself".
        statx(self.fd, CString::default(), libc::AT_EMPTY_PATH).await
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
    let (res, _, ()) = Op::new(Io::Close { fd }, ()).await;
    if res < 0 {
        Err(os_err(res))
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
        FileType(self.mode & libc::S_IFMT)
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

async fn statx(dir: RawFd, path: CString, flags: i32) -> io::Result<Metadata> {
    // SAFETY: all-zero is a valid `statx` (plain integers).
    let out: Box<libc::statx> = Box::new(unsafe { mem::zeroed() });
    let mask = libc::STATX_TYPE | libc::STATX_MODE | libc::STATX_SIZE | libc::STATX_MTIME;
    let mut resources = (path, out);
    let io = Io::Statx {
        dir,
        path: resources.0.as_ptr(),
        flags,
        mask,
        out: &mut *resources.1,
    };
    let (res, _, (_path, out)) = Op::new(io, resources).await;
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
    statx(libc::AT_FDCWD, c_path(path.as_ref())?, 0).await
}

/// Like [`metadata`], but describes a symbolic link itself instead of what it
/// points at.
pub async fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    statx(
        libc::AT_FDCWD,
        c_path(path.as_ref())?,
        libc::AT_SYMLINK_NOFOLLOW,
    )
    .await
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
    let io = Io::UnlinkAt {
        path: path.as_ptr(),
        flags: 0,
    };
    path_op(path, io).await.map(drop)
}

/// Removes an empty directory.
pub async fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let io = Io::UnlinkAt {
        path: path.as_ptr(),
        flags: libc::AT_REMOVEDIR,
    };
    path_op(path, io).await.map(drop)
}

/// Creates a directory (permissions `0o777` before the umask).
pub async fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = c_path(path.as_ref())?;
    let io = Io::MkDirAt {
        path: path.as_ptr(),
        mode: 0o777,
    };
    path_op(path, io).await.map(drop)
}

/// Renames a file or directory, replacing `to` if it exists.
pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    let paths = (c_path(from.as_ref())?, c_path(to.as_ref())?);
    let io = Io::RenameAt {
        from: paths.0.as_ptr(),
        to: paths.1.as_ptr(),
    };
    let (res, _, _paths) = Op::new(io, paths).await;
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
        self.0 == libc::S_IFREG
    }

    /// `true` for a directory.
    pub fn is_dir(&self) -> bool {
        self.0 == libc::S_IFDIR
    }

    /// `true` for a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.0 == libc::S_IFLNK
    }

    /// The type as the `S_IFMT` bits of `st_mode` (`S_IFREG`, `S_IFDIR`, ...).
    pub fn mode(&self) -> u32 {
        self.0
    }

    /// The type a directory entry reports, if the filesystem reports one.
    fn from_dirent(d_type: u8) -> Option<Self> {
        let mode = match d_type {
            libc::DT_REG => libc::S_IFREG,
            libc::DT_DIR => libc::S_IFDIR,
            libc::DT_LNK => libc::S_IFLNK,
            libc::DT_FIFO => libc::S_IFIFO,
            libc::DT_SOCK => libc::S_IFSOCK,
            libc::DT_CHR => libc::S_IFCHR,
            libc::DT_BLK => libc::S_IFBLK,
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
/// microseconds; on a network filesystem it can hold up the worker for a
/// round trip.
pub async fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let path = path.as_ref();
    let dir = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(path)
        .await?;
    Ok(ReadDir {
        dir,
        path: Arc::from(path),
        buf: Vec::new(),
        pos: 0,
        yield_first: false,
        done: false,
    })
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
}

impl ReadDir {
    /// The next entry, or `None` when there are no more. Cancel safe: if the
    /// future is dropped, no entry is lost.
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
            self.read_batch()?;
        }
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
        // SAFETY: the kernel writes at most `capacity` bytes into the buffer.
        let n = unsafe {
            libc::syscall(
                libc::SYS_getdents64,
                self.dir.fd,
                self.buf.as_mut_ptr(),
                self.buf.capacity(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the kernel wrote `n` bytes, no more than the capacity.
        unsafe { self.buf.set_len(n as usize) };
        self.pos = 0;
        self.yield_first = true;
        self.done = n == 0;
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
    pub async fn file_type(&self) -> io::Result<FileType> {
        match self.file_type {
            Some(file_type) => Ok(file_type),
            None => Ok(self.metadata().await?.file_type()),
        }
    }

    /// Metadata of the entry itself, like [`symlink_metadata`] on
    /// [`path`](Self::path).
    pub async fn metadata(&self) -> io::Result<Metadata> {
        symlink_metadata(self.path()).await
    }
}
