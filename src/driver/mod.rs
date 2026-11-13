//! The I/O drivers: io_uring, and epoll where io_uring cannot be used.
//!
//! Callers describe an operation with [`Io`] (or [`MultiIo`] for the
//! multishot ones) and never see which driver runs it:
//!
//! * [`uring`] submits it to the worker's io_uring and the kernel completes
//!   it. Everything a tick produces reaches the kernel in one
//!   `io_uring_enter`.
//! * [`epoll`] tries the system call at once, without blocking. If the
//!   descriptor is not ready it waits for readiness through epoll and tries
//!   again. File operations, for which a regular file is never "ready", run
//!   on a small pool of blocking threads ([`blocking`]).
//!
//! Both keep the same contract towards [`Op`](crate::op::Op): the memory an
//! operation points at stays with the driver until the operation can no
//! longer touch it, so dropping a future early never frees memory that the
//! kernel, or a blocking thread, may still write to.
//!
//! Completions look the same from both drivers. A multishot receive reports
//! its pool buffer in the flags (`buffer_id`), exactly as io_uring does, so
//! the socket code above has a single path.

mod blocking;
mod epoll;
mod uring;

use std::any::Any;
use std::io;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll, Waker};
use std::time::Duration;

use crate::buf::{BufPool, RecvBuf};
use crate::error::Code;

pub(crate) use epoll::refill_budget;

/// Which I/O driver a worker uses: io_uring, the default, or epoll where
/// io_uring is not available (see [`Builder::driver`](crate::Builder::driver)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DriverKind {
    /// Completion-based I/O through one io_uring per worker (Linux 6.0 or
    /// newer). The fastest, and the default when the kernel allows it.
    IoUring,
    /// Readiness-based I/O through one epoll instance per worker, for
    /// machines where io_uring is blocked or missing: Docker's default
    /// seccomp profile, gVisor, the `kernel.io_uring_disabled` sysctl, or a
    /// kernel older than 6.0. File operations run on a pool of blocking
    /// threads. Every API behaves the same; it costs more system calls.
    Epoll,
}

impl DriverKind {
    /// `"io_uring"` or `"epoll"`, as accepted by `GIONG_DRIVER`.
    pub const fn name(self) -> &'static str {
        match self {
            DriverKind::IoUring => "io_uring",
            DriverKind::Epoll => "epoll",
        }
    }
}

impl std::fmt::Display for DriverKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Who picked a worker's driver: see [`DriverChoice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChosenBy {
    /// [`Builder::driver`](crate::Builder::driver) named it.
    Builder,
    /// The `GIONG_DRIVER` environment variable named it.
    Environment,
    /// Nothing named a driver, so giong chose: io_uring where it works,
    /// epoll elsewhere.
    Automatic,
}

/// A bug in the running kernel that giong works around: see
/// [`DriverChoice::kernel_workarounds`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum KernelWorkaround {
    /// The kernel refuses to register an io_uring buffer ring whose reserved
    /// fields are zero, as every correct caller sends them, and accepts one
    /// whose reserved fields are not. Ubuntu's 6.8 kernels from 6.8.0-135
    /// on carry this bug, as do the AWS, Azure, GCP and Oracle 6.8 kernels
    /// and the 6.8 HWE kernel of Ubuntu 22.04 built from them: a backport of
    /// "io_uring/kbuf: use mem_is_zero()" inverted the check. giong first confirms the inversion with a probe
    /// that changes nothing, then registers with a reserved field set, and
    /// io_uring works at full speed. Without this, those kernels would put
    /// giong on epoll.
    BufferRingReservedCheck,
}

impl KernelWorkaround {
    /// A stable short name: `"buffer-ring-reserved-check"`.
    pub const fn name(self) -> &'static str {
        match self {
            KernelWorkaround::BufferRingReservedCheck => "buffer-ring-reserved-check",
        }
    }

    /// One sentence on the bug.
    pub const fn description(self) -> &'static str {
        match self {
            KernelWorkaround::BufferRingReservedCheck => {
                "this kernel refuses io_uring buffer rings with zeroed reserved fields (an \
                 inverted check, as in Ubuntu's 6.8.0-135 and later), so giong registers its \
                 ring with a reserved field set"
            }
        }
    }
}

impl std::fmt::Display for KernelWorkaround {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// An io_uring feature that giong uses where the kernel has it: see
/// [`DriverChoice::io_uring_features`]. Each one is probed when a worker
/// starts; without it giong does the same work the older way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IoUringFeature {
    /// Stream receives fill pool buffers piece by piece instead of taking a
    /// whole buffer each (a buffer ring with `IOU_PBUF_RING_INC`, Linux
    /// 6.12). Many connections that each send a little then share a few
    /// buffers, which keeps the memory a busy worker touches small.
    PackedBuffers,
}

impl IoUringFeature {
    /// Every feature, in the order [`DriverChoice::io_uring_features`] lists
    /// them.
    pub const ALL: &'static [IoUringFeature] = &[IoUringFeature::PackedBuffers];

    /// A stable short name, such as `"packed-buffers"`.
    pub const fn name(self) -> &'static str {
        match self {
            IoUringFeature::PackedBuffers => "packed-buffers",
        }
    }
}

impl std::fmt::Display for IoUringFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `false` if `GIONG_IO_URING_OFF` turns `feature` off. A test and benchmark
/// hook, like `GIONG_RING_TIER`: a comma-separated list of feature names
/// ([`IoUringFeature::name`]), or `all`. Unknown names are ignored.
pub(crate) fn feature_enabled(feature: IoUringFeature) -> bool {
    match std::env::var("GIONG_IO_URING_OFF") {
        Err(_) => true,
        Ok(off) => !off
            .split(',')
            .map(str::trim)
            .any(|name| name == "all" || name == feature.name()),
    }
}

/// Which I/O driver a worker runs on, who chose it, and why io_uring is not
/// used when giong fell back to epoll. Read it with
/// [`driver_choice`](crate::driver_choice) on the worker, or
/// [`Handle::driver_choice`](crate::Handle::driver_choice) from any thread;
/// `giong-mcp`'s `overview` shows it too.
///
/// Its [`Display`](std::fmt::Display) form is one line for a log:
///
/// ```text
/// io_uring (chosen automatically), using packed-buffers
/// epoll (chosen automatically): io_uring cannot be used here: GIONG-E0001: giong needs io_uring, …
/// io_uring (chosen automatically), working around a kernel bug: this kernel refuses …
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DriverChoice {
    /// The driver the worker runs on.
    pub kind: DriverKind,
    /// Who chose it.
    pub chosen_by: ChosenBy,
    /// When giong fell back to epoll by itself: the error that setting up
    /// io_uring gave, as a runtime that names io_uring would have returned
    /// it (`GIONG-E0001: …`, with how to fix it). `None` otherwise.
    pub io_uring_error: Option<String>,
    /// The code of that error, when giong raised it.
    pub io_uring_error_code: Option<Code>,
    /// Bugs of this kernel that giong works around to run io_uring.
    pub kernel_workarounds: Vec<KernelWorkaround>,
    /// The io_uring features the worker uses (empty on epoll): those the
    /// kernel has, less any that `GIONG_IO_URING_OFF` turns off.
    pub io_uring_features: Vec<IoUringFeature>,
}

impl std::fmt::Display for DriverChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let by = match self.chosen_by {
            ChosenBy::Builder => "named by Builder::driver",
            ChosenBy::Environment => "named by GIONG_DRIVER",
            ChosenBy::Automatic => "chosen automatically",
        };
        write!(f, "{} ({by})", self.kind)?;
        for (i, feature) in self.io_uring_features.iter().enumerate() {
            let sep = if i == 0 { ", using " } else { ", " };
            write!(f, "{sep}{feature}")?;
        }
        if let Some(e) = &self.io_uring_error {
            write!(f, ": io_uring cannot be used here: {e}")?;
        }
        for (i, w) in self.kernel_workarounds.iter().enumerate() {
            let sep = if i == 0 {
                ", working around a kernel bug: "
            } else {
                "; "
            };
            write!(f, "{sep}{}", w.description())?;
        }
        Ok(())
    }
}

/// How the event loop wants to enter the kernel.
pub(crate) enum Park {
    /// Submit and collect completions without blocking.
    Poll,
    /// Block until a completion arrives, or for at most the given duration.
    Wait(Option<Duration>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MultiKind {
    Accept,
    Recv,
}

/// Result of polling a multishot operation.
pub(crate) enum MultiPoll {
    /// A result without data: an accepted descriptor, or a receive that
    /// ended (0: end of stream; negative: an error).
    Item(i32),
    /// Received bytes, in a pool buffer or on the heap.
    Data(RecvBuf),
    /// Nothing queued and nothing in flight: the caller must arm it.
    NeedArm,
    Pending,
}

/// How [`Driver::start`] dealt with a single-shot operation.
pub(crate) enum Start {
    /// Finished at once (epoll): `(result, flags)`.
    Done(i32, u32),
    /// In flight under this key: poll it with [`Driver::poll_op`].
    Submitted(usize),
}

/// The completion flag that says a pool buffer holds the data
/// (`IORING_CQE_F_BUFFER`); the buffer id sits in the upper 16 bits.
const F_BUFFER: u32 = 1;
/// More completions will follow for this operation (`IORING_CQE_F_MORE`).
const F_MORE: u32 = 2;
const BUFFER_SHIFT: u32 = 16;

/// The packed buffer ring keeps the rest of the buffer this completion
/// names for the next receive (`IORING_CQE_F_BUF_MORE`, Linux 6.12).
const F_BUF_MORE: u32 = 1 << 4;

/// The pool buffer a receive completion names, if any.
pub(crate) fn buffer_id(flags: u32) -> Option<u16> {
    (flags & F_BUFFER != 0).then_some((flags >> BUFFER_SHIFT) as u16)
}

/// A single-shot operation. Every pointer must stay valid, at the same
/// address, until the operation completes: [`Op`](crate::op::Op) owns what
/// they point at and hands it to the driver if it is dropped early.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Io {
    /// `connect(2)` a stream socket.
    Connect {
        fd: RawFd,
        addr: *const libc::sockaddr,
        len: libc::socklen_t,
    },
    /// A one-off receive on a socket into the caller's buffer.
    Recv { fd: RawFd, buf: *mut u8, len: u32 },
    /// `send(2)`.
    Send {
        fd: RawFd,
        buf: *const u8,
        len: u32,
        flags: i32,
    },
    /// `sendmsg(2)`.
    SendMsg {
        fd: RawFd,
        msg: *const libc::msghdr,
        flags: u32,
    },
    /// A one-off `recvmsg(2)`.
    RecvMsg { fd: RawFd, msg: *mut libc::msghdr },
    /// A `read(2)` from a descriptor that has readiness, such as an eventfd
    /// or a pipe.
    ReadReady { fd: RawFd, buf: *mut u8, len: u32 },
    /// `pread(2)` from a file (a plain `read` on a pipe or a device).
    Read {
        fd: RawFd,
        buf: *mut u8,
        len: u32,
        offset: u64,
    },
    /// `pwrite(2)` to a file (a plain `write` on a pipe or a device).
    Write {
        fd: RawFd,
        buf: *const u8,
        len: u32,
        offset: u64,
    },
    /// `openat(2)` relative to the working directory.
    OpenAt {
        path: *const libc::c_char,
        flags: i32,
        mode: u32,
    },
    /// `close(2)`, reporting its result.
    Close { fd: RawFd },
    /// `statx(2)`.
    Statx {
        dir: RawFd,
        path: *const libc::c_char,
        flags: i32,
        mask: u32,
        out: *mut libc::statx,
    },
    /// `fsync(2)`, or `fdatasync(2)` when `datasync`.
    Fsync { fd: RawFd, datasync: bool },
    /// `fallocate(2)` with mode 0.
    Fallocate { fd: RawFd, offset: u64, len: u64 },
    /// `unlinkat(2)` relative to the working directory.
    UnlinkAt {
        path: *const libc::c_char,
        flags: i32,
    },
    /// `mkdirat(2)` relative to the working directory.
    MkDirAt {
        path: *const libc::c_char,
        mode: u32,
    },
    /// `renameat(2)`, both paths relative to the working directory.
    RenameAt {
        from: *const libc::c_char,
        to: *const libc::c_char,
    },
}

/// A multishot operation: it keeps producing results until it is dropped.
#[derive(Clone, Copy, Debug)]
pub(crate) enum MultiIo {
    /// Accepted connections, as close-on-exec descriptors.
    Accept { fd: RawFd },
    /// Data into the worker's pool buffers.
    Recv { fd: RawFd },
    /// Datagrams into pool buffers, each laid out as io_uring's
    /// `io_uring_recvmsg_out` header, then the sender's address in a field
    /// of `hdr.msg_namelen` bytes, then the payload.
    RecvMsg { fd: RawFd, hdr: *const libc::msghdr },
}

/// A worker's I/O driver.
// One per worker, created once and never moved: boxing the larger variant
// would only add an indirection to every operation.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Driver {
    Uring(uring::Uring),
    Epoll(epoll::Epoll),
}

/// `GIONG_DRIVER` when the builder did not choose: `None` if unset or
/// empty, an error naming the value if it is not a driver.
pub(crate) fn kind_from_env() -> io::Result<Option<DriverKind>> {
    match std::env::var("GIONG_DRIVER") {
        Err(_) => Ok(None),
        Ok(v) => match v.trim() {
            "" | "auto" => Ok(None),
            "io_uring" | "io-uring" | "uring" => Ok(Some(DriverKind::IoUring)),
            "epoll" => Ok(Some(DriverKind::Epoll)),
            other => Err(crate::error::raise(
                Code::InvalidConfig,
                format!("GIONG_DRIVER is {other:?}, which is not a driver"),
                "set GIONG_DRIVER to io_uring or epoll, or unset it to use io_uring where the \
                 kernel allows it and epoll elsewhere.",
            )),
        },
    }
}

/// `true` if an io_uring setup error means "io_uring cannot be used here",
/// rather than a mistake in the configuration: the automatic choice then
/// falls back to epoll.
fn io_uring_unusable(e: &io::Error) -> bool {
    matches!(
        crate::error::code(e),
        Some(Code::IoUringUnavailable | Code::KernelTooOld | Code::BufferRingRefused)
    ) || matches!(
        e.raw_os_error(),
        Some(libc::EPERM | libc::EACCES | libc::ENOSYS | libc::EINVAL)
    )
}

/// Says once per process, on stderr, that giong fell back to epoll and why:
/// the fallback costs speed, and nobody would know otherwise. Naming a
/// driver takes the choice away from giong, and the line with it.
fn report_fallback(e: &io::Error) {
    static REPORTED: AtomicBool = AtomicBool::new(false);
    if !REPORTED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "giong: running on the epoll driver, because io_uring cannot be used here: {e} \
             (name a driver with Builder::driver or GIONG_DRIVER to choose one yourself; \
             giong::driver_choice() tells what a worker got)"
        );
    }
}

impl Driver {
    /// Sets up the driver `choice` names, or io_uring with epoll as the
    /// fallback when it is `None`. `chosen_by` says where `choice` came from.
    pub(crate) fn new(
        choice: Option<DriverKind>,
        chosen_by: ChosenBy,
        entries: u32,
        pool: Rc<BufPool>,
        wake_fd: RawFd,
    ) -> io::Result<(Self, DriverChoice)> {
        let mut io_uring_error = None;
        let driver = match choice {
            Some(DriverKind::IoUring) => Driver::Uring(uring::Uring::new(entries, pool, wake_fd)?),
            Some(DriverKind::Epoll) => Driver::Epoll(epoll::Epoll::new(pool, wake_fd)?),
            None => match uring::Uring::new(entries, pool.clone(), wake_fd) {
                Ok(ring) => Driver::Uring(ring),
                Err(e) if io_uring_unusable(&e) => {
                    let driver = Driver::Epoll(epoll::Epoll::new(pool, wake_fd)?);
                    report_fallback(&e);
                    io_uring_error = Some(e);
                    driver
                }
                Err(e) => return Err(e),
            },
        };
        let choice = DriverChoice {
            kind: driver.kind(),
            chosen_by,
            io_uring_error_code: io_uring_error.as_ref().and_then(crate::error::code),
            io_uring_error: io_uring_error.map(|e| e.to_string()),
            kernel_workarounds: match &driver {
                Driver::Uring(d) => d.workarounds().to_vec(),
                Driver::Epoll(_) => Vec::new(),
            },
            io_uring_features: match &driver {
                Driver::Uring(d) => d.features().to_vec(),
                Driver::Epoll(_) => Vec::new(),
            },
        };
        Ok((driver, choice))
    }

    pub(crate) fn kind(&self) -> DriverKind {
        match self {
            Driver::Uring(_) => DriverKind::IoUring,
            Driver::Epoll(_) => DriverKind::Epoll,
        }
    }

    /// Starts a single-shot operation; `waker` is woken when it can be
    /// polled again.
    pub(crate) fn start(&mut self, io: &Io, waker: &Waker) -> Start {
        match self {
            Driver::Uring(d) => Start::Submitted(d.submit_op(io, waker)),
            Driver::Epoll(d) => d.start(io, waker),
        }
    }

    /// Polls a single-shot operation, consuming its entry when it is done.
    pub(crate) fn poll_op(&mut self, key: usize, waker: &Waker) -> Poll<(i32, u32)> {
        match self {
            Driver::Uring(d) => d.poll_op(key, waker),
            Driver::Epoll(d) => d.poll_op(key, waker),
        }
    }

    /// The future of a single-shot operation was dropped before its result
    /// was collected. `data` are the resources the operation may still use.
    pub(crate) fn cancel_op(&mut self, key: usize, data: Option<Box<dyn Any>>) {
        match self {
            Driver::Uring(d) => d.cancel_op(key, data),
            Driver::Epoll(d) => d.cancel_op(key, data),
        }
    }

    pub(crate) fn new_multi(&mut self, kind: MultiKind) -> usize {
        match self {
            Driver::Uring(d) => d.new_multi(kind),
            Driver::Epoll(d) => d.new_multi(kind),
        }
    }

    pub(crate) fn arm_multi(&mut self, key: usize, io: MultiIo) {
        match self {
            Driver::Uring(d) => d.arm_multi(key, io),
            Driver::Epoll(d) => d.arm_multi(key, io),
        }
    }

    pub(crate) fn poll_multi(&mut self, key: usize, waker: &Waker) -> MultiPoll {
        match self {
            Driver::Uring(d) => d.poll_multi(key, waker),
            Driver::Epoll(d) => d.poll_multi(key, waker),
        }
    }

    /// The owner of a multishot operation (listener, stream) goes away.
    pub(crate) fn drop_multi(&mut self, key: usize) {
        match self {
            Driver::Uring(d) => d.drop_multi(key),
            Driver::Epoll(d) => d.drop_multi(key),
        }
    }

    /// Moves received-but-unread data out of the buffer pool (io_uring; the
    /// epoll driver never queues pool buffers).
    pub(crate) fn evacuate(&mut self) {
        if let Driver::Uring(d) = self {
            d.evacuate();
        }
    }

    /// Closes a descriptor the caller gives up.
    pub(crate) fn close_fd(&mut self, fd: RawFd) {
        match self {
            Driver::Uring(d) => d.close_fd(fd),
            Driver::Epoll(d) => d.close_fd(fd),
        }
    }

    /// Hands queued SQEs to the kernel now (see `Uring::submit_queued`).
    #[cfg(giong_tsan)]
    pub(crate) fn submit_queued(&mut self) {
        match self {
            Driver::Uring(d) => d.submit_queued(),
            Driver::Epoll(_) => {}
        }
    }

    pub(crate) fn pool(&self) -> &Rc<BufPool> {
        match self {
            Driver::Uring(d) => d.pool(),
            Driver::Epoll(d) => d.pool(),
        }
    }

    /// Makes sure a receive of this kind can get a pool buffer before it is
    /// armed (a datagram receive, or a stream one). `false` if none is free.
    pub(crate) fn ensure_recv_buffer(&self, datagram: bool) -> bool {
        match self {
            Driver::Uring(d) => d.ensure_recv_buffer(datagram),
            Driver::Epoll(d) => d.pool().ensure_published(None),
        }
    }

    /// `true` if there is something to collect without waiting.
    pub(crate) fn has_completions(&mut self) -> bool {
        match self {
            Driver::Uring(d) => d.has_completions(),
            Driver::Epoll(d) => d.has_completions(),
        }
    }

    /// Enters the kernel, then wakes the tasks whose operations can make
    /// progress. Returns resources that must be dropped outside the driver.
    pub(crate) fn park(&mut self, mode: Park) -> Vec<Box<dyn Any>> {
        match self {
            Driver::Uring(d) => d.park(mode),
            Driver::Epoll(d) => d.park(mode),
        }
    }

    /// Cancels what is left and waits for it. `false` means something may
    /// still use the driver's memory: the caller must leak it.
    pub(crate) fn shutdown(&mut self) -> bool {
        match self {
            Driver::Uring(d) => d.shutdown(),
            Driver::Epoll(d) => d.shutdown(),
        }
    }

    pub(crate) fn take_graveyard(&mut self) -> Vec<Box<dyn Any>> {
        match self {
            Driver::Uring(d) => d.take_graveyard(),
            Driver::Epoll(d) => d.take_graveyard(),
        }
    }

    /// Operations whose final completion is still outstanding.
    pub(crate) fn inflight(&self) -> usize {
        match self {
            Driver::Uring(d) => d.inflight(),
            Driver::Epoll(d) => d.inflight(),
        }
    }

    /// Number of tracked operations (used by tests to detect leaks).
    pub(crate) fn tracked_ops(&self) -> usize {
        match self {
            Driver::Uring(d) => d.tracked_ops(),
            Driver::Epoll(d) => d.tracked_ops(),
        }
    }
}
