//! Files on Windows.
//!
//! Reads and writes are overlapped: they go to the kernel from the worker
//! and complete through its completion port, like socket I/O, so the worker
//! thread never waits for the disk. Windows has no overlapped form of the
//! other operations (opens, metadata, renames, removals, flushes, directory
//! listings), so those run on a pool of blocking threads and report to the
//! worker's port, with the same API and the same cancellation rules.
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
//! This is the API of the Unix module without what only Unix has
//! (permission bits, inode numbers, `open(2)` flags), and with handles in
//! place of descriptors.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io;
use std::marker::PhantomData;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::SystemTime;

use crate::buf::{IoBuf, IoBufMut};
use crate::driver::{BlockingJob, Io};
use crate::error::{raise, Code};
use crate::kbox::KernelBox;
use crate::op::Op;
use crate::runtime;
use crate::sys;

fn os_err(res: i32) -> io::Error {
    io::Error::from_raw_os_error(-res)
}

/// `path`, owned, for a blocking thread; refused if it holds a NUL.
fn owned_path(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().encode_wide().any(|unit| unit == 0) {
        return Err(raise(
            Code::InvalidPath,
            format!("the path {path:?} contains a NUL character"),
            "remove the NUL character; Windows paths cannot contain one.",
        ));
    }
    Ok(path.to_path_buf())
}

/// A call for a blocking thread and the place its result goes.
struct Call<F, T> {
    call: Option<F>,
    out: Option<io::Result<T>>,
}

impl<F, T> BlockingJob for Call<F, T>
where
    F: FnOnce() -> io::Result<T> + Send,
    T: Send,
{
    fn run(&mut self) -> i32 {
        if let Some(call) = self.call.take() {
            self.out = Some(call());
        }
        0
    }
}

/// Runs `call` on a blocking thread. `keep` is what the call's pointers
/// point into: it is handed back with the result, and if the future is
/// dropped the driver keeps it until the call has returned.
///
/// # Safety
///
/// Every pointer `call` uses must point into memory owned by `keep` that
/// does not move when `keep` does (see [`Op::new`]).
async unsafe fn blocking_with<K, F, T>(keep: K, call: F) -> (io::Result<T>, K)
where
    K: 'static,
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let job = KernelBox::new(Call {
        call: Some(call),
        out: None,
    });
    let io = Io::Blocking { job: job.as_ptr() };
    // SAFETY: the job is in a `KernelBox`; the caller answers for `keep`.
    let (res, _, (job, keep)) = unsafe { Op::new(io, (job, keep)) }.await;
    if res < 0 {
        return (Err(os_err(res)), keep);
    }
    // SAFETY: the job has reported, so nothing else uses it any more.
    let out = unsafe { (*job.as_ptr()).out.take() };
    (
        out.unwrap_or_else(|| Err(io::ErrorKind::Interrupted.into())),
        keep,
    )
}

/// Runs `call` on a blocking thread.
async fn blocking<F, T>(call: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    // SAFETY: nothing is kept, so `call` has no pointers into it.
    unsafe { blocking_with((), call) }.await.0
}

/// A pointer a blocking call reads or writes through.
struct SendPtr(*mut u8);

// SAFETY: the memory is owned by what `blocking_with` keeps, which nothing
// else touches until the call has returned.
unsafe impl Send for SendPtr {}

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
    custom_flags: u32,
}

impl OpenOptions {
    /// All options off.
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            custom_flags: 0,
        }
    }

    /// Read access.
    pub fn read(mut self, read: bool) -> Self {
        self.read = read;
        self
    }

    /// Write access.
    pub fn write(mut self, write: bool) -> Self {
        self.write = write;
        self
    }

    /// Every write goes to the end of the file, whatever offset it names.
    pub fn append(mut self, append: bool) -> Self {
        self.append = append;
        self
    }

    /// Empties the file when it is opened.
    pub fn truncate(mut self, truncate: bool) -> Self {
        self.truncate = truncate;
        self
    }

    /// Creates the file if it does not exist.
    pub fn create(mut self, create: bool) -> Self {
        self.create = create;
        self
    }

    /// Creates the file, and fails with `AlreadyExists` if it exists.
    pub fn create_new(mut self, create_new: bool) -> Self {
        self.create_new = create_new;
        self
    }

    /// Extra `CreateFileW` flags, for example `FILE_FLAG_SEQUENTIAL_SCAN`,
    /// as in the standard library's `OpenOptionsExt::custom_flags`. The file
    /// is always opened for overlapped I/O.
    pub fn custom_flags(mut self, flags: u32) -> Self {
        self.custom_flags = flags;
        self
    }

    /// The same checks as on Unix, so that a wrong combination fails with
    /// the same coded error everywhere.
    fn check(&self) -> io::Result<()> {
        if !self.read && !self.write && !self.append {
            return Err(invalid_options(
                "the open options ask for neither read, write nor append access",
                "add .read(true), .write(true) or .append(true)",
            ));
        }
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
        Ok(())
    }

    /// Opens `path` with these options.
    ///
    /// If the future is dropped once the open has started, the handle it
    /// may still produce is closed. What the open did to the file stays
    /// done: a dropped `create` or `truncate` may have created or emptied
    /// the file, in any runtime.
    ///
    /// Not cancel safe when the options create or truncate (see above); cancel
    /// safe otherwise: a dropped open leaves no handle behind.
    pub async fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        self.check()?;
        let path = owned_path(path.as_ref())?;
        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write && !self.append)
            .append(self.append)
            .truncate(self.truncate && !self.create_new)
            .create(self.create)
            .create_new(self.create_new)
            .custom_flags(self.custom_flags | sys::FILE_FLAG_OVERLAPPED);
        // A file the caller never receives is closed where it is dropped:
        // with the operation's resources.
        let file = blocking(move || options.open(path)).await?;
        let handle = file.into_raw_handle();
        // A console (`CONIN$`, `CONOUT$`) ignores the overlapped flag and
        // cannot join the completion port: its calls block, so they go to
        // the blocking threads.
        Ok(File::from_handle(handle, !sys::is_char_device(handle)))
    }
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

#[cold]
fn invalid_options(
    message: impl Into<std::borrow::Cow<'static, str>>,
    fix: impl Into<std::borrow::Cow<'static, str>>,
) -> io::Error {
    raise(Code::InvalidOpenOptions, message, fix)
}

/// Refuses an offset no file can have, as the Unix module does, so that
/// code which computes a wrong offset fails the same way everywhere.
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
            "file offset {offset} is above {}, the largest a file can have",
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
    handle: RawHandle,
    /// Opened for overlapped I/O, so reads and writes go through the
    /// completion port. A file from [`File::from_std`] may not be: its reads
    /// and writes run on the blocking threads.
    overlapped: bool,
    _not_send: PhantomData<*const ()>,
}

impl File {
    /// Opens an existing file for reading.
    ///
    /// Cancel safe: if the future is dropped, the handle the open may still
    /// produce is closed, and nothing else changes.
    pub async fn open<P: AsRef<Path>>(path: P) -> io::Result<Self> {
        OpenOptions::new().read(true).open(path).await
    }

    /// Opens a file for writing, creating it if needed and emptying it if it
    /// exists.
    ///
    /// Not cancel safe: if the future is dropped, the file may have been
    /// created or emptied (the handle is closed either way).
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

    /// Wraps a file opened by the standard library. Such a file is usually
    /// not open for overlapped I/O, so its reads and writes run on the
    /// blocking threads; a file opened through [`File::options`] reads and
    /// writes through the worker's completion port.
    pub fn from_std(file: std::fs::File) -> Self {
        Self::from_handle(file.into_raw_handle(), false)
    }

    fn from_handle(handle: RawHandle, overlapped: bool) -> Self {
        Self {
            handle,
            overlapped,
            _not_send: PhantomData,
        }
    }

    /// The handle, as a standard file that does not close it.
    fn borrowed(&self) -> mem::ManuallyDrop<std::fs::File> {
        // SAFETY: the handle is open while `self` lives, and the
        // `ManuallyDrop` keeps the temporary from closing it.
        mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(self.handle) })
    }

    /// A second handle to the file, for a blocking thread: it stays valid
    /// there even if this `File` is closed meanwhile.
    fn second_handle(&self) -> io::Result<std::fs::File> {
        self.borrowed().try_clone()
    }

    /// Reads at `offset` into `buf` (from its start, up to its capacity),
    /// returning the buffer with its length set to the bytes read. `Ok(0)`
    /// means `offset` is at or past the end of the file.
    ///
    /// This may return fewer bytes than there is room for.
    ///
    /// Dropping the future is harmless for a regular file. If the handle is
    /// a pipe or a device, a read that is dropped while in flight may still
    /// take data that arrives before the kernel has processed the
    /// cancellation, and that data is lost.
    ///
    /// Fails with `GIONG-E0202` if `offset` is above `i64::MAX`.
    ///
    /// Cancel safe for a regular file, as said above; not for a pipe or a
    /// device.
    pub async fn read_at<B: IoBufMut>(&self, mut buf: B, offset: u64) -> (io::Result<usize>, B) {
        if let Err(e) = check_offset(offset) {
            return (Err(e), buf);
        }
        let len = buf.capacity().min(i32::MAX as usize) as u32;
        let res = if self.overlapped {
            let io = Io::Read {
                fd: self.handle,
                buf: buf.as_mut_ptr(),
                len,
                offset,
            };
            // SAFETY: an `IoBufMut`'s bytes stay put when it moves.
            let (res, _, b) = unsafe { Op::new(io, buf) }.await;
            buf = b;
            if res < 0 {
                Err(os_err(res))
            } else {
                Ok(res as usize)
            }
        } else {
            let file = match self.second_handle() {
                Ok(file) => file,
                Err(e) => return (Err(e), buf),
            };
            let to = SendPtr(buf.as_mut_ptr());
            let call = move || {
                let to = to;
                // SAFETY: `to` has room for `len` bytes (the buffer's
                // capacity) and is kept until this returns.
                unsafe { sys::read_at(file.as_raw_handle(), to.0, len, offset) }
            };
            // SAFETY: an `IoBufMut`'s bytes stay put when it moves.
            let (res, b) = unsafe { blocking_with(buf, call) }.await;
            buf = b;
            res
        };
        if let Ok(n) = res {
            // SAFETY: the kernel wrote `n` bytes at the start of the buffer.
            unsafe { buf.set_init(n) };
        }
        (res, buf)
    }

    /// Writes some bytes of `buf` at `offset`, returning how many were
    /// written.
    ///
    /// Fails with `GIONG-E0202` if `offset` is above `i64::MAX`.
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
        let len = (buf.len() - skip).min(i32::MAX as usize) as u32;
        // SAFETY: `skip <= len`, so the pointer stays within the buffer.
        let ptr = unsafe { buf.as_ptr().add(skip) };
        if self.overlapped {
            let io = Io::Write {
                fd: self.handle,
                buf: ptr,
                len,
                offset,
            };
            // SAFETY: `ptr` points into `buf`, an `IoBuf`, whose bytes stay
            // put when it moves.
            let (res, _, buf) = unsafe { Op::new(io, buf) }.await;
            if res < 0 {
                (Err(os_err(res)), buf)
            } else {
                (Ok(res as usize), buf)
            }
        } else {
            let file = match self.second_handle() {
                Ok(file) => file,
                Err(e) => return (Err(e), buf),
            };
            let from = SendPtr(ptr.cast_mut());
            let call = move || {
                let from = from;
                // SAFETY: `from` names `len` bytes of the buffer, which is
                // kept until this returns; they are only read.
                unsafe { sys::write_at(file.as_raw_handle(), from.0, len, offset) }
            };
            // SAFETY: as above.
            unsafe { blocking_with(buf, call) }.await
        }
    }

    /// Flushes data and metadata to the storage device
    /// (`FlushFileBuffers`).
    ///
    /// Cancel safe: if the future is dropped, the flush may or may not have
    /// happened, and calling again does it.
    pub async fn sync_all(&self) -> io::Result<()> {
        let file = self.second_handle()?;
        blocking(move || file.sync_all()).await
    }

    /// Flushes the file's data to the storage device. Windows has no call
    /// that leaves the metadata out, so this does what
    /// [`sync_all`](Self::sync_all) does.
    ///
    /// Cancel safe, like [`sync_all`](Self::sync_all).
    pub async fn sync_data(&self) -> io::Result<()> {
        let file = self.second_handle()?;
        blocking(move || file.sync_data()).await
    }

    /// Makes sure the file has room for `len` bytes at `offset`: a file
    /// that ends before `offset + len` is extended to there, and Windows
    /// reserves the space. A file that is long enough is left alone.
    ///
    /// The file must be open for writing. Calls of `allocate` may overlap
    /// each other. Do not run one while writes extend the same file past
    /// `offset + len`: Windows sets a file's length rather than growing it,
    /// so a write that lands there between the check and the change would
    /// be cut off.
    ///
    /// Fails with `GIONG-E0202` if `offset + len` is above `i64::MAX`.
    ///
    /// Cancel safe: if the future is dropped, the space may or may not have
    /// been reserved, and calling again does it.
    pub async fn allocate(&self, offset: u64, len: u64) -> io::Result<()> {
        if len == 0 {
            return Ok(());
        }
        let Some(end) = offset
            .checked_add(len)
            .filter(|&end| end <= i64::MAX as u64)
        else {
            return Err(raise(
                Code::OffsetOutOfRange,
                format!(
                    "the {len} bytes at file offset {offset} end above {}, the largest offset \
                     a file can have",
                    i64::MAX
                ),
                "check the arithmetic that computed the offset and the length for a \
                 wrap-around",
            ));
        };
        let file = self.second_handle()?;
        blocking(move || {
            // Windows sets a length where Unix grows a file, so two calls
            // that overlapped could leave the shorter length: one checks,
            // the other grows the file further, the first then sets its
            // own. One at a time, in the whole process, none can.
            static GROWING: Mutex<()> = Mutex::new(());
            let _one_at_a_time = GROWING.lock().unwrap_or_else(|e| e.into_inner());
            // A write of nothing changes nothing, and fails as any write
            // does on a file that is not open for writing: so does this
            // call, whether or not the file has to grow.
            // SAFETY: no byte is read through the pointer.
            unsafe {
                sys::write_at(
                    file.as_raw_handle(),
                    std::ptr::NonNull::dangling().as_ptr(),
                    0,
                    0,
                )
            }?;
            if file.metadata()?.len() < end {
                file.set_len(end)?;
            }
            Ok(())
        })
        .await
    }

    /// The file's metadata.
    ///
    /// Cancel safe: if the future is dropped, nothing changes.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        let file = self.second_handle()?;
        blocking(move || file.metadata().map(Metadata::from_std)).await
    }

    /// The file's length in bytes, read with a direct system call (it does
    /// not wait for the disk).
    #[allow(clippy::len_without_is_empty)] // `is_empty` is right below.
    pub fn len(&self) -> io::Result<u64> {
        Ok(self.borrowed().metadata()?.len())
    }

    /// `true` if the file has no bytes.
    pub fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Closes the file and reports the error, if any. Dropping a `File`
    /// closes it too, without the error.
    ///
    /// Cancel safe: the handle is closed even if the future is dropped.
    pub async fn close(self) -> io::Result<()> {
        let handle = self.handle;
        mem::forget(self);
        // SAFETY: the operation has no pointers.
        let (res, _, ()) = unsafe { Op::new(Io::Close { fd: handle }, ()) }.await;
        if res < 0 {
            Err(os_err(res))
        } else {
            Ok(())
        }
    }
}

impl AsRawHandle for File {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle
    }
}

impl Drop for File {
    fn drop(&mut self) {
        let handle = self.handle;
        // Through the current worker's driver, which forgets the handle
        // too; directly if there is none.
        let closed = runtime::try_with_worker(|w| match w.driver.try_borrow_mut() {
            Ok(mut driver) => driver.close_fd(handle),
            Err(_) => w.defer_close(handle),
        })
        .is_some();
        if !closed {
            // SAFETY: the handle is ours and is closed exactly once.
            drop(unsafe { std::fs::File::from_raw_handle(handle) });
        }
    }
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("File")
            .field("handle", &self.handle)
            .finish()
    }
}

/// What a file is and how large; see [`metadata`] and [`File::metadata`].
#[derive(Clone, Debug)]
pub struct Metadata {
    len: u64,
    file_type: FileType,
    modified: SystemTime,
}

impl Metadata {
    fn from_std(meta: std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            file_type: FileType::from_std(meta.file_type()),
            modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        }
    }

    /// The length in bytes.
    #[allow(clippy::len_without_is_empty)] // `is_empty` is right below.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// `true` if the length is 0.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// `true` for a regular file.
    pub fn is_file(&self) -> bool {
        self.file_type.is_file()
    }

    /// `true` for a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type.is_dir()
    }

    /// `true` for a symbolic link (only [`symlink_metadata`] reports one).
    pub fn is_symlink(&self) -> bool {
        self.file_type.is_symlink()
    }

    /// The kind of file.
    pub fn file_type(&self) -> FileType {
        self.file_type
    }

    /// When the contents last changed.
    pub fn modified(&self) -> SystemTime {
        self.modified
    }
}

/// The metadata of the file at `path`, following symbolic links.
///
/// Cancel safe: if the future is dropped, nothing changes.
pub async fn metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    let path = owned_path(path.as_ref())?;
    blocking(move || std::fs::metadata(path).map(Metadata::from_std)).await
}

/// The metadata of the file at `path`; of the link itself if it is a
/// symbolic link.
///
/// Cancel safe: if the future is dropped, nothing changes.
pub async fn symlink_metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
    let path = owned_path(path.as_ref())?;
    blocking(move || std::fs::symlink_metadata(path).map(Metadata::from_std)).await
}

/// Reads a whole file.
///
/// Cancel safe: if the future is dropped, nothing changes and the file is
/// closed.
pub async fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    let file = File::open(path).await?;
    // The size is only a hint: a file may grow while it is being read.
    let hint = file.len().unwrap_or(0) as usize;
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
    let path = owned_path(path.as_ref())?;
    blocking(move || std::fs::remove_file(path)).await
}

/// Removes an empty directory.
///
/// Not cancel safe, like [`remove_file`].
pub async fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = owned_path(path.as_ref())?;
    blocking(move || std::fs::remove_dir(path)).await
}

/// Creates a directory.
///
/// Not cancel safe: if the future is dropped, the directory may have been
/// created, and calling again then fails with `AlreadyExists`.
pub async fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let path = owned_path(path.as_ref())?;
    blocking(move || std::fs::create_dir(path)).await
}

/// Renames a file or directory, replacing `to` if it is a file that exists.
///
/// Not cancel safe: if the future is dropped, the rename may have happened, and
/// calling again then fails with `NotFound`.
pub async fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
    let (from, to) = (owned_path(from.as_ref())?, owned_path(to.as_ref())?);
    blocking(move || std::fs::rename(from, to)).await
}

/// The kind of a file: regular file, directory, symbolic link, or something
/// else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileType(Kind);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

impl FileType {
    fn from_std(file_type: std::fs::FileType) -> Self {
        Self(if file_type.is_symlink() {
            Kind::Symlink
        } else if file_type.is_dir() {
            Kind::Dir
        } else if file_type.is_file() {
            Kind::File
        } else {
            Kind::Other
        })
    }

    /// `true` for a regular file.
    pub fn is_file(&self) -> bool {
        self.0 == Kind::File
    }

    /// `true` for a directory.
    pub fn is_dir(&self) -> bool {
        self.0 == Kind::Dir
    }

    /// `true` for a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.0 == Kind::Symlink
    }
}

/// How many entries one trip to a blocking thread fetches.
const DIR_BATCH: usize = 128;

/// Lists the entries of a directory, without `.` and `..`, in the order the
/// filesystem keeps them.
///
/// ```no_run
/// # async fn demo() -> std::io::Result<()> {
/// let mut dir = giong::fs::read_dir("C:\\Windows").await?;
/// while let Some(entry) = dir.next_entry().await? {
///     if entry.file_type().await?.is_file() {
///         println!("{}", entry.path().display());
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// The entries are fetched on a blocking thread, 128 at a time, so the
/// worker never waits for the disk or for a network share.
///
/// Cancel safe: if the future is dropped, nothing changes and the directory is
/// closed.
pub async fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
    let path: Arc<Path> = Arc::from(owned_path(path.as_ref())?);
    let dir = path.clone();
    let listing = blocking(move || std::fs::read_dir(dir)).await?;
    Ok(ReadDir {
        path,
        shared: Arc::new(Mutex::new(Listing {
            dir: Some(listing),
            ready: VecDeque::new(),
            failed: None,
        })),
    })
}

/// What a [`ReadDir`] shares with the blocking thread that fills it: the
/// entries wait here, so a batch fetched for a future that was dropped is
/// not lost.
struct Listing {
    /// `None` once the directory has been read to its end.
    dir: Option<std::fs::ReadDir>,
    ready: VecDeque<(OsString, Option<FileType>)>,
    failed: Option<io::Error>,
}

impl Listing {
    /// Fetches the next batch, unless one is waiting.
    fn fill(&mut self) {
        if !self.ready.is_empty() || self.failed.is_some() {
            return;
        }
        let Some(dir) = self.dir.as_mut() else {
            return;
        };
        while self.ready.len() < DIR_BATCH {
            match dir.next() {
                Some(Ok(entry)) => {
                    // Windows returns the kind with the name: no extra call.
                    let file_type = entry.file_type().ok().map(FileType::from_std);
                    self.ready.push_back((entry.file_name(), file_type));
                }
                Some(Err(e)) => {
                    self.failed = Some(e);
                    return;
                }
                None => {
                    self.dir = None;
                    return;
                }
            }
        }
    }
}

/// The entries of a directory; see [`read_dir`].
pub struct ReadDir {
    path: Arc<Path>,
    shared: Arc<Mutex<Listing>>,
}

impl ReadDir {
    /// The next entry, or `None` after the last one.
    ///
    /// Cancel safe: if the future is dropped, no entry is lost; the next
    /// call returns the entry this one would have.
    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        loop {
            // The lock is held by a blocking thread only while it fetches a
            // batch (for a future that was dropped); the worker never waits
            // for it.
            match self.shared.try_lock() {
                Ok(mut listing) => {
                    if let Some((name, file_type)) = listing.ready.pop_front() {
                        return Ok(Some(DirEntry {
                            dir: self.path.clone(),
                            name,
                            file_type,
                        }));
                    }
                    if let Some(e) = listing.failed.take() {
                        return Err(e);
                    }
                    if listing.dir.is_none() {
                        return Ok(None);
                    }
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Poisoned(_)) => {
                    return Err(io::Error::other("reading the directory failed earlier"));
                }
            }
            let shared = self.shared.clone();
            blocking(move || {
                if let Ok(mut listing) = shared.lock() {
                    listing.fill();
                }
                Ok(())
            })
            .await?;
        }
    }
}

impl std::fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadDir").field("path", &self.path).finish()
    }
}

/// One entry of a directory; see [`ReadDir::next_entry`].
#[derive(Clone, Debug)]
pub struct DirEntry {
    dir: Arc<Path>,
    name: OsString,
    file_type: Option<FileType>,
}

impl DirEntry {
    /// The entry's name, without the directory.
    pub fn file_name(&self) -> &OsStr {
        &self.name
    }

    /// The directory's path as given to [`read_dir`], joined with the name.
    pub fn path(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    /// The kind of the entry, without following a symbolic link. Windows
    /// returns it with the listing, so this does not go to the disk again.
    ///
    /// Cancel safe: if the future is dropped, nothing changes.
    pub async fn file_type(&self) -> io::Result<FileType> {
        match self.file_type {
            Some(file_type) => Ok(file_type),
            None => Ok(self.metadata().await?.file_type()),
        }
    }

    /// The entry's metadata, without following a symbolic link.
    ///
    /// Cancel safe: if the future is dropped, nothing changes.
    pub async fn metadata(&self) -> io::Result<Metadata> {
        symlink_metadata(self.path()).await
    }
}
