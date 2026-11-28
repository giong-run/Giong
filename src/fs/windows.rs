//! Files on Windows.
//!
//! Reads and writes are overlapped: they go to the kernel from the worker
//! and complete through its completion port, like socket I/O, so the worker
//! thread never waits for the disk. Windows has no overlapped form of the
//! other operations (opens, metadata, renames, removals, flushes, directory
//! listings), so those run on a pool of blocking threads and report to the
//! worker's port, with the same API and the same cancellation rules.
//!
//! Two calls that look overlapped can still wait for the disk, and run on
//! the blocking threads too: a write that extends the file (NTFS runs a
//! write past the data already written synchronously, even on an
//! overlapped handle), and the close of a file opened for writing (the file
//! system's cleanup and the filter drivers' scans run inside
//! `CloseHandle`). In CI either took up to 0.85 s, during which the worker
//! served nothing else.
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
//! This is the API of the Unix module without what only Unix has
//! (permission bits, inode numbers, `open(2)` flags), and with handles in
//! place of descriptors.

use std::cell::Cell;
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

/// Runs `call` on a blocking thread (also used by the DNS resolver to ask
/// the network adapters).
pub(crate) async fn blocking<F, T>(call: F) -> io::Result<T>
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
        let options = self.std_options(0);
        // A file the caller never receives is closed where it is dropped:
        // with the operation's resources.
        let file = blocking(move || options.open(path)).await?;
        Ok(File::from_std_opened(file, self.writes()))
    }

    /// Opens `path` beneath `root` with these options, refusing a path
    /// that could lead out of it (see [`Root`]).
    ///
    /// Cancel safe as [`open`](Self::open) is.
    pub async fn open_in<P: AsRef<Path>>(&self, root: &Root, path: P) -> io::Result<File> {
        self.check()?;
        let file = root
            .open_beneath(path.as_ref(), Want::File(self.clone()))
            .await?;
        Ok(File::from_std_opened(file, self.writes()))
    }

    /// The standard library's options for these, with `extra` flags.
    fn std_options(&self, extra: u32) -> std::fs::OpenOptions {
        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write && !self.append)
            .append(self.append)
            .truncate(self.truncate && !self.create_new)
            .create(self.create)
            .create_new(self.create_new)
            .custom_flags(self.custom_flags | sys::FILE_FLAG_OVERLAPPED | extra);
        options
    }

    /// How a file opened with these options is written to.
    fn writes(&self) -> Writes {
        if self.append {
            Writes::Append
        } else if self.write {
            Writes::At
        } else {
            Writes::None
        }
    }

    /// Whether opening with these options may create or empty a file.
    fn changes_the_file(&self) -> bool {
        self.create || self.create_new || self.truncate
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
    /// How the file was opened for writing.
    writes: Writes,
    /// Where a write starts to extend the file, as far as this handle
    /// knows: the length at open, then the end of the furthest write. A
    /// write that ends past it runs on a blocking thread, since NTFS runs it
    /// synchronously, overlapped handle or not.
    grows_from: Cell<u64>,
    _not_send: PhantomData<*const ()>,
}

/// How a [`File`] was opened for writing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Writes {
    /// Not at all.
    None,
    /// At the offsets the caller names.
    At,
    /// Always at the end: every write extends the file.
    Append,
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
        // How it was opened is not known: it may have been written to, so
        // it is closed as such.
        Self::from_handle(file.into_raw_handle(), false, Writes::At, 0)
    }

    /// A file `OpenOptions` opened, for overlapped I/O.
    fn from_std_opened(file: std::fs::File, writes: Writes) -> Self {
        // The length lives in memory with the open file: reading it does not
        // wait for the disk. Unknown, every write counts as extending.
        let len = match writes {
            Writes::At => file.metadata().map_or(0, |m| m.len()),
            Writes::None | Writes::Append => 0,
        };
        let handle = file.into_raw_handle();
        // A console (`CONIN$`, `CONOUT$`) ignores the overlapped flag and
        // cannot join the completion port: its calls block, so they go to
        // the blocking threads.
        Self::from_handle(handle, !sys::is_char_device(handle), writes, len)
    }

    fn from_handle(handle: RawHandle, overlapped: bool, writes: Writes, len: u64) -> Self {
        Self {
            handle,
            overlapped,
            writes,
            grows_from: Cell::new(len),
            _not_send: PhantomData,
        }
    }

    /// Whether `len` bytes written at `offset` may extend the file.
    fn extends(&self, offset: u64, len: u32) -> bool {
        self.writes == Writes::Append
            || offset.saturating_add(u64::from(len)) > self.grows_from.get()
    }

    /// The handle, as a standard file that does not close it.
    fn borrowed(&self) -> mem::ManuallyDrop<std::fs::File> {
        // SAFETY: the handle is open while `self` lives, and the
        // `ManuallyDrop` keeps the temporary from closing it.
        mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(self.handle) })
    }

    /// A second handle to the file, for a blocking thread or a send in
    /// flight: it stays valid there even if this `File` is closed meanwhile.
    pub(crate) fn second_handle(&self) -> io::Result<std::fs::File> {
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
        let (res, buf) = self.write_raw(buf, ptr, len, offset).await;
        if let Ok(n) = res {
            let end = offset.saturating_add(n as u64);
            if end > self.grows_from.get() {
                self.grows_from.set(end);
            }
        }
        (res, buf)
    }

    /// Writes the `len` bytes at `ptr`, inside `buf`, at `offset`.
    async fn write_raw<B: IoBuf>(
        &self,
        buf: B,
        ptr: *const u8,
        len: u32,
        offset: u64,
    ) -> (io::Result<usize>, B) {
        if self.overlapped && !self.extends(offset, len) {
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
        // A file written to may wait for the disk as it closes: that runs
        // on a blocking thread.
        let io = if self.writes == Writes::None {
            Io::Close { fd: handle }
        } else {
            Io::CloseFile { fd: handle }
        };
        mem::forget(self);
        // SAFETY: the operation has no pointers.
        let (res, _, ()) = unsafe { Op::new(io, ()) }.await;
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
        // A file written to may wait for the disk as it closes: that runs
        // on a blocking thread.
        let off_worker = self.writes != Writes::None;
        let closed = runtime::try_with_worker(|w| match w.driver.try_borrow_mut() {
            Ok(mut driver) if off_worker => driver.close_file(handle),
            Ok(mut driver) => driver.close_fd(handle),
            Err(_) if off_worker => w.defer_close_file(handle),
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

/// A directory that paths are opened beneath, and never outside of.
///
/// A path from a request, an upload or a configuration file can hold `..`,
/// be absolute, or reach a symbolic link or a junction someone placed in
/// the directory: joined to the directory and opened, it can lead anywhere
/// on the machine. A `Root` refuses, with `GIONG-E0205`, any path that is
/// absolute (with a drive, a share or a leading `\`), has a `..`
/// component, or goes through a symbolic link or a junction (wherever it
/// points): each component is opened without following reparse points,
/// and the directories on the way are held open, so that none can be
/// swapped for a link between the check and the open. The file opened is
/// then checked to be beneath the directory, where Windows says it is.
///
/// ```no_run
/// use giong::fs::Root;
///
/// # async fn demo(request_path: &str) -> std::io::Result<()> {
/// let public = Root::new("public").await?;
/// // "css\\site.css" opens; "..\\.env", "C:\\secrets" and a link to either
/// // are refused.
/// let file = public.open(request_path).await?;
/// # Ok(())
/// # }
/// ```
///
/// The directory itself may be reached through a link (`Root::new`
/// follows links like any open): only what is beneath it is checked. While
/// a `Root` holds it, the directory cannot be renamed or removed, so that
/// its path keeps naming the same directory. Like a [`File`], a `Root`
/// belongs to the worker that opened it.
pub struct Root {
    /// The directory, held open without `FILE_SHARE_DELETE`.
    _dir: std::fs::File,
    /// Its full path as Windows resolved it (`\\?\C:\...`): what is beneath
    /// is opened from there, so no name is read as a device (`CON`).
    full: Arc<PathBuf>,
    path: PathBuf,
    _not_send: PhantomData<*const ()>,
}

/// What an open beneath a root is for.
enum Want {
    /// A file, with these options.
    File(OpenOptions),
    /// A directory, held as a root of its own.
    Root,
    /// Only its metadata.
    Metadata,
}

/// `CreateFileW` flags: open a directory (`FILE_FLAG_BACKUP_SEMANTICS`)
/// and a reparse point itself instead of what it points at.
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
/// Access to read a file's attributes only.
const FILE_READ_ATTRIBUTES: u32 = 0x0080;
/// Others may read and write, but not rename or remove.
const SHARE_ALL_BUT_DELETE: u32 = 0x1 | 0x2;
/// `ERROR_DIRECTORY`: the path is not a directory.
const ERROR_DIRECTORY: i32 = 267;

impl Root {
    /// Opens the directory `dir` as a root.
    ///
    /// Cancel safe: if the future is dropped, the handle the open may still
    /// produce is closed, and nothing else changes.
    pub async fn new<P: AsRef<Path>>(dir: P) -> io::Result<Self> {
        let path = owned_path(dir.as_ref())?;
        let at = path.clone();
        let (dir, full) = blocking(move || {
            let dir = open_directory(&at, 0)?;
            let full = final_path(&dir)?;
            Ok((dir, full))
        })
        .await?;
        Ok(Self {
            _dir: dir,
            full: Arc::new(full),
            path,
            _not_send: PhantomData,
        })
    }

    /// Opens the file at `path` beneath the root for reading.
    ///
    /// Cancel safe: if the future is dropped, the handle the open may still
    /// produce is closed, and nothing else changes.
    pub async fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        OpenOptions::new().read(true).open_in(self, path).await
    }

    /// Opens the file at `path` beneath the root for writing, creating it
    /// if needed and emptying it if it exists.
    ///
    /// Not cancel safe: if the future is dropped, the file may have been
    /// created or emptied (the handle is closed either way).
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
    /// root. A symbolic link or a junction there is refused, as everywhere
    /// under a root.
    ///
    /// Cancel safe: it changes nothing.
    pub async fn metadata<P: AsRef<Path>>(&self, path: P) -> io::Result<Metadata> {
        let file = self.open_beneath(path.as_ref(), Want::Metadata).await?;
        blocking(move || file.metadata().map(Metadata::from_std)).await
    }

    /// Opens the directory at `path` beneath this root as a root of its
    /// own, for paths beneath it.
    ///
    /// Cancel safe: if the future is dropped, the handle the open may still
    /// produce is closed, and nothing else changes.
    pub async fn open_root<P: AsRef<Path>>(&self, path: P) -> io::Result<Root> {
        let path = path.as_ref();
        let dir = self.open_beneath(path, Want::Root).await?;
        let (dir, full) = blocking(move || {
            let full = final_path(&dir)?;
            Ok((dir, full))
        })
        .await?;
        Ok(Root {
            _dir: dir,
            full: Arc::new(full),
            path: self.path.join(path),
            _not_send: PhantomData,
        })
    }

    /// The directory's path, as given to [`new`](Self::new) (joined with
    /// the path given to [`open_root`](Self::open_root)).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What is at `path` beneath the root, opened for `want`.
    async fn open_beneath(&self, path: &Path, want: Want) -> io::Result<std::fs::File> {
        use std::path::Component;
        let mut parts = Vec::new();
        for part in owned_path(path)?.components() {
            match part {
                Component::Prefix(_) | Component::RootDir => {
                    return Err(outside(&self.path, path, "is absolute"))
                }
                Component::ParentDir => {
                    return Err(outside(&self.path, path, "has a .. component"))
                }
                Component::CurDir => {}
                Component::Normal(name) => parts.push(name.to_os_string()),
            }
        }
        let full = Arc::clone(&self.full);
        let (root, rel) = (self.path.clone(), path.to_path_buf());
        blocking(move || walk_beneath(&full, &parts, &want, &root, &rel)).await
    }
}

impl std::fmt::Debug for Root {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Root").field("path", &self.path).finish()
    }
}

/// Opens the directory at `path`, without letting it be renamed or
/// removed while it is open; `extra` flags added.
fn open_directory(path: &Path, extra: u32) -> io::Result<std::fs::File> {
    let dir = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL_BUT_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | extra)
        .open(path)?;
    if !dir.metadata()?.is_dir() {
        return Err(io::Error::from_raw_os_error(ERROR_DIRECTORY));
    }
    Ok(dir)
}

/// The blocking part of an open beneath a root: each directory on the
/// way is opened without following reparse points and held open (so that
/// it cannot be renamed or swapped) until the end, and a symbolic link or
/// a junction anywhere is refused.
fn walk_beneath(
    full: &Path,
    parts: &[OsString],
    want: &Want,
    root: &Path,
    rel: &Path,
) -> io::Result<std::fs::File> {
    let link = || outside(root, rel, "goes through a symbolic link or a junction");
    let is_link = |file: &std::fs::File| -> io::Result<bool> {
        // A name surrogate (a symbolic link, a junction), as the standard
        // library tells them; other reparse points (OneDrive placeholders,
        // deduplicated files) are files.
        Ok(file.metadata()?.file_type().is_symlink())
    };
    let mut at = full.to_path_buf();
    let mut held = Vec::with_capacity(parts.len());
    let (last, dirs) = match parts.split_last() {
        Some((last, dirs)) => (Some(last), dirs),
        None => (None, &[][..]),
    };
    for part in dirs {
        at.push(part);
        let dir = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(SHARE_ALL_BUT_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&at)?;
        if is_link(&dir)? {
            return Err(link());
        }
        if !dir.metadata()?.is_dir() {
            return Err(io::Error::from_raw_os_error(ERROR_DIRECTORY));
        }
        held.push(dir);
    }
    if let Some(last) = last {
        at.push(last);
        // What is there now, if anything: a link is refused before any
        // open that could follow it.
        let existing = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(SHARE_ALL_BUT_DELETE | 0x4)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&at);
        match existing {
            Ok(file) if is_link(&file)? => return Err(link()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    let file = match want {
        // An open that creates or empties never follows a reparse point,
        // so that a link put there since the look above is opened (and
        // refused) itself, and nothing outside is created or emptied.
        Want::File(options) if options.changes_the_file() => options
            .std_options(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&at)?,
        Want::File(options) => options.std_options(0).open(&at)?,
        Want::Root => open_directory(&at, FILE_FLAG_OPEN_REPARSE_POINT)?,
        Want::Metadata => std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(SHARE_ALL_BUT_DELETE | 0x4)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(&at)?,
    };
    if is_link(&file)? {
        return Err(link());
    }
    // Where Windows says the file is: beneath the root, or refused.
    if !final_path(&file)?.starts_with(full) {
        return Err(outside(root, rel, "leads outside the directory"));
    }
    drop(held);
    Ok(file)
}

/// The full path of an open file, as Windows resolved it.
fn final_path(file: &std::fs::File) -> io::Result<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };
    let mut buf = vec![0u16; 512];
    loop {
        // SAFETY: an open handle, and a buffer of `buf.len()` units.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n < buf.len() {
            buf.truncate(n);
            return Ok(PathBuf::from(OsString::from_wide(&buf)));
        }
        // Too small: `n` is the size needed, the NUL included.
        buf.resize(n, 0);
    }
}

#[cold]
fn outside(root: &Path, path: &Path, why: &str) -> io::Error {
    raise(
        Code::OutsideRoot,
        format!("the path {path:?} {why}, so it is not opened beneath the root {root:?}"),
        "give a path relative to the root, without .. and without symbolic links or junctions \
         on the way; to open files in a directory a link leads to, open a Root on that directory",
    )
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
    read_all(File::open(path).await?).await
}

/// The whole of `file`, from its start.
async fn read_all(file: File) -> io::Result<Vec<u8>> {
    // The size is only a hint: a file may grow while it is being read.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_may_extend_the_file_is_told_from_one_that_cannot() {
        let dir = std::env::temp_dir().join(format!("giong-extends-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        crate::block_on(async {
            // A new file: every write extends it until bytes are written.
            let f = File::create(&path).await.unwrap();
            assert!(f.extends(0, 1));
            let (res, _) = f.write_all_at(vec![7u8; 8192], 0).await;
            res.unwrap();
            assert!(!f.extends(0, 8192));
            assert!(!f.extends(4096, 4096));
            assert!(f.extends(4096, 4097));
            f.close().await.unwrap();

            // Opened again for writing, it knows its length.
            let f = File::options().write(true).open(&path).await.unwrap();
            assert!(!f.extends(0, 8192));
            assert!(f.extends(8191, 2));
            // A write in place goes through the completion port and keeps
            // the bytes around it.
            let (res, _) = f.write_all_at(vec![1u8; 4], 100).await;
            res.unwrap();
            drop(f);

            // Appending always extends.
            let f = File::options().append(true).open(&path).await.unwrap();
            assert!(f.extends(0, 1));
            f.close().await.unwrap();

            let back = std::fs::read(&path).unwrap();
            assert_eq!(back.len(), 8192);
            assert_eq!(&back[96..108], &[7, 7, 7, 7, 1, 1, 1, 1, 7, 7, 7, 7]);
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_written_to_is_closed_whether_closed_or_dropped() {
        let dir = std::env::temp_dir().join(format!("giong-close-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        crate::block_on(async {
            for i in 0..64 {
                let path = dir.join(format!("f{i}"));
                let f = File::create(&path).await.unwrap();
                let (res, _) = f.write_all_at(vec![i as u8; 100], 0).await;
                res.unwrap();
                if i % 2 == 0 {
                    f.close().await.unwrap();
                } else {
                    drop(f);
                }
            }
        });
        // Closed on blocking threads: each file can be removed once they
        // are done, which a handle still open would refuse.
        let give_up = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => break,
                Err(e) if std::time::Instant::now() < give_up => {
                    let _ = e;
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("a file is still open after 10 s: {e}"),
            }
        }
    }
}
