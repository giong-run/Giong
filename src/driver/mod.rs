//! The I/O drivers: io_uring, and epoll where io_uring cannot be used, on
//! Linux; kqueue on macOS.
//!
//! Callers describe an operation with [`Io`] (or [`MultiIo`] for the
//! multishot ones) and never see which driver runs it:
//!
//! * [`uring`] submits it to the worker's io_uring and the kernel completes
//!   it. Everything a tick produces reaches the kernel in one
//!   `io_uring_enter`.
//! * [`readiness`] tries the system call at once, without blocking. If the
//!   descriptor is not ready it waits for readiness through epoll (Linux)
//!   or kqueue (macOS) and tries again. File operations, for which a regular
//!   file is never "ready", run on a small pool of blocking threads
//!   ([`blocking`]).
//!
//! Both keep the same contract towards [`Op`](crate::op::Op): the memory an
//! operation points at stays with the driver until the operation can no
//! longer touch it, so dropping a future early never frees memory that the
//! kernel, or a blocking thread, may still write to.
//!
//! Completions look the same from both drivers. A multishot receive reports
//! its pool buffer in the flags (`buffer_id`), exactly as io_uring does, so
//! the socket code above has a single path.

#[cfg(any(unix, windows))]
mod blocking;
mod budget;
#[cfg(target_os = "linux")]
mod epoll;
#[cfg(windows)]
mod iocp;
#[cfg(any(target_os = "macos", giong_libkqueue))]
mod kqueue;
#[cfg(unix)]
mod readiness;
#[cfg(target_os = "linux")]
mod uring;

use std::any::Any;
use std::cell::Cell;
use std::io;
use std::rc::Rc;
#[cfg(target_os = "linux")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Poll, Waker};
use std::time::Duration;

use crate::buf::{BufPool, RecvBuf};
use crate::error::Code;
use crate::sys::{MsgHdr, PathChar, RawFile, RawSock, SockAddr, SockLen};

pub(crate) use budget::{refill_budget, spend_budget, spend_or_yield};

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
    /// Readiness-based I/O through one kqueue per worker: the driver on
    /// macOS, which is supported for development, not for speed. It is the
    /// epoll driver's logic over kqueue; file operations run on a pool of
    /// blocking threads.
    Kqueue,
    /// Completion-based I/O through one I/O completion port per worker: the
    /// driver on Windows, and the only one there.
    Iocp,
}

impl DriverKind {
    /// `"io_uring"`, `"epoll"`, `"kqueue"` or `"iocp"`, as accepted by
    /// `GIONG_DRIVER`.
    pub const fn name(self) -> &'static str {
        match self {
            DriverKind::IoUring => "io_uring",
            DriverKind::Epoll => "epoll",
            DriverKind::Kqueue => "kqueue",
            DriverKind::Iocp => "iocp",
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
    /// The worker enters the kernel through its thread's registered ring
    /// table (`IORING_REGISTER_RING_FDS`, Linux 5.18) instead of the ring's
    /// descriptor, which spares the kernel looking the descriptor up on
    /// every `io_uring_enter`.
    RegisteredRing,
    /// Large sends on TCP sockets from bytes that cannot change
    /// (`&'static [u8]`, `Rc<[u8]>`, `Arc<[u8]>`) go out without a copy into
    /// the kernel (`IORING_OP_SEND_ZC` and `IORING_OP_SENDMSG_ZC`, Linux
    /// 6.2): the network card reads them, and the write returns as soon as
    /// they are queued, as a copying send does. A vectored write may add up
    /// to 4 KiB from other buffers (an HTTP head), which are copied aside.
    /// Used for sends from 16 KiB on; a socket whose data the kernel copies
    /// anyway (loopback, a card without scatter-gather) or cannot send this
    /// way (kTLS) goes back to plain sends. Bytes that can change once the
    /// write returns (a `Vec`) are always copied: the kernel reads
    /// zero-copy bytes until the peer has acknowledged them.
    ZeroCopySend,
    /// Inside a scope with a deadline ([`crate::task`]), operations that
    /// wait on a peer (connect, one-off reads, waiting for a writable
    /// socket, waiting on a pipe) carry the deadline into the kernel as a
    /// linked timeout (`IORING_OP_LINK_TIMEOUT`, with
    /// `IOSQE_CQE_SKIP_SUCCESS` so that it adds no completion). The kernel
    /// cancels them at the deadline by itself, even while the worker is
    /// busy. Without it, the deadline's timer ends them at the next turn of
    /// the event loop.
    LinkedDeadlines,
}

impl IoUringFeature {
    /// Every feature, in the order [`DriverChoice::io_uring_features`] lists
    /// them.
    pub const ALL: &'static [IoUringFeature] = &[
        IoUringFeature::PackedBuffers,
        IoUringFeature::RegisteredRing,
        IoUringFeature::ZeroCopySend,
        IoUringFeature::LinkedDeadlines,
    ];

    /// A stable short name, such as `"packed-buffers"`.
    pub const fn name(self) -> &'static str {
        match self {
            IoUringFeature::PackedBuffers => "packed-buffers",
            IoUringFeature::RegisteredRing => "registered-ring",
            IoUringFeature::ZeroCopySend => "zero-copy-send",
            IoUringFeature::LinkedDeadlines => "linked-deadlines",
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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // io_uring only
pub(crate) fn feature_enabled(feature: IoUringFeature) -> bool {
    match std::env::var("GIONG_IO_URING_OFF") {
        Err(_) => true,
        Ok(off) => !off
            .split(',')
            .map(str::trim)
            .any(|name| name == "all" || name == feature.name()),
    }
}

/// Sends of at least this many bytes go out without a copy where
/// [`IoUringFeature::ZeroCopySend`] is in use. Below it, copying the data is
/// cheaper than pinning its pages and waiting for the kernel to let go of
/// them. `GIONG_ZERO_COPY_MIN` (bytes) changes it for benchmarks.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // io_uring only
const ZERO_COPY_MIN: usize = 16 * 1024;

/// [`ZERO_COPY_MIN`], or `GIONG_ZERO_COPY_MIN` where it is set.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // io_uring only
pub(crate) fn zero_copy_min() -> usize {
    std::env::var("GIONG_ZERO_COPY_MIN")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(ZERO_COPY_MIN)
}

/// What the kernel may still read after a zero-copy send has its result:
/// owners that keep the bytes alive and unchanged, and the bytes it was
/// cheaper to copy than to wait for. The driver keeps it until the send's
/// notification, then sets `copied` if the kernel copied the data after all
/// (as over loopback), so the socket stops asking.
#[allow(dead_code)] // `owners` and `scratch` are only kept, never read.
pub(crate) struct ZeroCopyHold {
    pub(crate) owners: Vec<Box<dyn Any>>,
    pub(crate) scratch: Vec<u8>,
    pub(crate) copied: Rc<Cell<bool>>,
}

/// Set by the io_uring driver in the flags of a zero-copy send whose data
/// the kernel copied after all (`IORING_NOTIF_USAGE_ZC_COPIED`). Bit 31 of a
/// CQE's flags is part of the buffer id, which sends never have.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // io_uring only
pub(crate) const ZC_COPIED: u32 = 1 << 31;

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
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // io_uring only
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
        fd: RawSock,
        addr: *const SockAddr,
        len: SockLen,
    },
    /// A one-off receive on a socket into the caller's buffer.
    Recv { fd: RawSock, buf: *mut u8, len: u32 },
    /// `send(2)`. With `zero_copy` the io_uring driver sends without a copy
    /// where it can ([`IoUringFeature::ZeroCopySend`]). The kernel then
    /// reads the buffer until a notification that comes once the peer has
    /// acknowledged the data: an `Op` with a [`ZeroCopyHold`] completes with
    /// the result and leaves the hold with the driver until then; one
    /// without completes only at the notification, with [`ZC_COPIED`] in
    /// its flags if the kernel copied the data after all.
    Send {
        fd: RawSock,
        buf: *const u8,
        len: u32,
        flags: i32,
        zero_copy: bool,
    },
    /// Waits until a socket can take more data (`POLLOUT`), for a caller
    /// that writes to it itself and got `EAGAIN`. Completes with a
    /// non-negative value when the caller should try again.
    Writable { fd: RawSock },
    /// `sendmsg(2)`; `zero_copy` as for [`Io::Send`].
    SendMsg {
        fd: RawSock,
        msg: *const MsgHdr,
        flags: u32,
        zero_copy: bool,
    },
    /// `sendto(2)`: one datagram to `addr` from a socket that need not be
    /// connected. The io_uring driver submits it only where
    /// [`Driver::sends_to`] says the kernel takes it (`IORING_OP_SEND` with
    /// a destination address, Linux 6.1). Sent with `MSG_NOSIGNAL`; no
    /// other flags, so that every operation stays as small as before.
    SendTo {
        fd: RawSock,
        buf: *const u8,
        len: u32,
        addr: *const SockAddr,
        addr_len: SockLen,
    },
    /// A one-off `recvmsg(2)` with `flags` (such as `MSG_CMSG_CLOEXEC`).
    RecvMsg {
        fd: RawSock,
        msg: *mut MsgHdr,
        flags: i32,
    },
    /// Waits until a descriptor has something to read (`POLLIN`), such as a
    /// pidfd whose process exited, or a non-blocking pipe that returned
    /// `EAGAIN`. Completes with a non-negative value when the caller should
    /// look again. Used by `giong::process`, Linux only for now.
    #[cfg(unix)]
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Readable { fd: RawFile },
    /// A `read(2)` from a descriptor that has readiness, such as an eventfd
    /// or a pipe.
    ReadReady { fd: RawFile, buf: *mut u8, len: u32 },
    /// `pread(2)` from a file (a plain `read` on a pipe or a device).
    Read {
        fd: RawFile,
        buf: *mut u8,
        len: u32,
        offset: u64,
    },
    /// `pwrite(2)` to a file (a plain `write` on a pipe or a device).
    Write {
        fd: RawFile,
        buf: *const u8,
        len: u32,
        offset: u64,
    },
    /// `openat(2)` relative to the working directory.
    OpenAt {
        path: *const PathChar,
        flags: i32,
        mode: u32,
    },
    /// An open that stays beneath the directory `dir` (see `fs::Root`):
    /// `openat2(2)` on Linux, with the flags, the mode and how the path is
    /// resolved in `how`; `openat(2)` with `O_NOFOLLOW_ANY` on macOS.
    #[cfg(unix)]
    OpenBeneath {
        dir: RawFile,
        path: *const PathChar,
        how: *const crate::sys::OpenHow,
    },
    /// `close(2)`, reporting its result.
    Close { fd: RawFile },
    /// `statx(2)` on Linux, `fstatat(2)` elsewhere (see `sys::stat_at`).
    #[cfg(unix)]
    Statx {
        dir: RawFile,
        path: *const PathChar,
        flags: i32,
        mask: u32,
        out: *mut crate::sys::StatBuf,
    },
    /// `fsync(2)`, or `fdatasync(2)` when `datasync`.
    Fsync { fd: RawFile, datasync: bool },
    /// `fallocate(2)` with mode 0.
    Fallocate { fd: RawFile, offset: u64, len: u64 },
    /// `unlinkat(2)` relative to the working directory.
    UnlinkAt { path: *const PathChar, flags: i32 },
    /// `mkdirat(2)` relative to the working directory.
    MkDirAt { path: *const PathChar, mode: u32 },
    /// `renameat(2)`, both paths relative to the working directory.
    RenameAt {
        from: *const PathChar,
        to: *const PathChar,
    },
    /// Runs `job` on a blocking thread: the file operations Windows has no
    /// overlapped form of (opens, metadata, renames, directory listings).
    /// The result is what [`BlockingJob::run`] returns.
    #[cfg(windows)]
    Blocking { job: *mut dyn BlockingJob },
    /// `TransmitFile`: sends `len` bytes of `file` from `offset` on the
    /// stream socket `fd`, straight from the system's file cache.
    #[cfg(windows)]
    SendFile {
        fd: RawSock,
        file: RawFile,
        len: u32,
        offset: u64,
    },
}

/// What an [`Io::Blocking`] runs. It lives in the operation's resources, so
/// the driver keeps it until the job has reported, and only the blocking
/// thread touches it in between.
#[cfg(windows)]
pub(crate) trait BlockingJob: Send {
    /// Does the work; 0 or more on success, a negated error code on failure.
    fn run(&mut self) -> i32;
}

impl Io {
    /// `true` for the socket transfers that wait until they can proceed, so
    /// that `EAGAIN` from them is never an answer: io_uring returns it when
    /// the socket is non-blocking and it stopped waiting (see
    /// [`Op`](crate::op::Op)), and the epoll driver never does.
    /// `true` for operations whose result, when not negative, is a new
    /// descriptor that whoever collects the result must close. When nobody
    /// does (the future was dropped), the driver closes it.
    pub(crate) fn result_is_fd(&self) -> bool {
        #[cfg(unix)]
        if matches!(self, Io::OpenBeneath { .. }) {
            return true;
        }
        matches!(self, Io::OpenAt { .. })
    }

    /// `true` for operations that run to the end once submitted, even when
    /// their future is dropped: a close the caller gave up on must still
    /// close, or the descriptor stays open with nobody owning it.
    pub(crate) fn finishes_when_dropped(&self) -> bool {
        matches!(self, Io::Close { .. })
    }

    /// `true` for the operations that wait on a peer for as long as it
    /// takes: inside a scope with a deadline ([`crate::task`]) the io_uring
    /// driver links a kernel timeout to them. Transfers that usually finish
    /// at once (sends, file I/O) are left alone: a linked timeout costs each
    /// operation about 0.3 µs, and the deadline's timer still ends them.
    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn links_deadline(&self) -> bool {
        matches!(
            self,
            Io::Connect { .. }
                | Io::Recv { .. }
                | Io::RecvMsg { .. }
                | Io::Writable { .. }
                | Io::Readable { .. }
                | Io::ReadReady { .. }
        )
    }

    pub(crate) fn waits_when_not_ready(&self) -> bool {
        match *self {
            Io::Recv { .. } | Io::RecvMsg { .. } => true,
            Io::Send { flags, .. } => crate::sys::send_waits(flags),
            Io::SendMsg { flags, .. } => crate::sys::send_waits(flags as i32),
            Io::SendTo { .. } => true,
            _ => false,
        }
    }

    /// For a zero-copy send, the same send with a copy, which is what to
    /// submit when the kernel refuses to send without one (`EOPNOTSUPP`, as
    /// kernel TLS does). `None` for every other operation.
    pub(crate) fn copying(&self) -> Option<Io> {
        match *self {
            Io::Send {
                fd,
                buf,
                len,
                flags,
                zero_copy: true,
            } => Some(Io::Send {
                fd,
                buf,
                len,
                flags,
                zero_copy: false,
            }),
            Io::SendMsg {
                fd,
                msg,
                flags,
                zero_copy: true,
            } => Some(Io::SendMsg {
                fd,
                msg,
                flags,
                zero_copy: false,
            }),
            _ => None,
        }
    }
}

/// Stops the worker on a kernel error its event loop cannot go on after
/// (a completion the kernel dropped would leave an operation waiting for
/// ever). The panic carries `GIONG-E0005` with the cause and what to do.
#[cold]
#[track_caller]
pub(crate) fn event_loop_failed(call: &str, err: io::Error) -> ! {
    let fix = match err.raw_os_error() {
        #[cfg(target_os = "linux")]
        Some(libc::EBADR) => {
            "the kernel dropped completions because it could not allocate memory for them \
             (EBADR): the machine ran out of memory. Give the process more memory, or run \
             fewer connections per worker."
        }
        #[cfg(unix)]
        Some(libc::ENOMEM) => {
            "the kernel is out of memory. Give the process more memory, or run fewer \
             connections per worker."
        }
        _ => {
            "please report it at https://github.com/giong-run/Giong/issues with this message \
             and your kernel version (uname -r)."
        }
    };
    let err = crate::error::Error::new(
        crate::error::Code::Unexpected,
        format!("{call} failed, so this worker cannot go on: {err}"),
        fix,
    );
    panic!("{err}")
}

/// A multishot operation: it keeps producing results until it is dropped.
#[derive(Clone, Copy, Debug)]
pub(crate) enum MultiIo {
    /// Accepted connections, as close-on-exec descriptors.
    Accept { fd: RawSock },
    /// Data into the worker's pool buffers. `once`: a single receive of at
    /// most one buffer (io_uring), for a stream that reads large bodies with
    /// `recv_into` and should leave them in the socket until then.
    Recv {
        fd: RawSock,
        // Read by io_uring only: the readiness drivers receive once per
        // readiness anyway.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        once: bool,
    },
    /// Datagrams into pool buffers, each laid out as io_uring's
    /// `io_uring_recvmsg_out` header, then the sender's address in a field
    /// of `hdr.msg_namelen` bytes, then the payload.
    RecvMsg { fd: RawSock, hdr: *const MsgHdr },
}

/// A worker's I/O driver.
// One per worker, created once and never moved: boxing the larger variant
// would only add an indirection to every operation.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Driver {
    #[cfg(target_os = "linux")]
    Uring(uring::Uring),
    #[cfg(target_os = "linux")]
    Epoll(readiness::Readiness<epoll::Epoll>),
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    Kqueue(readiness::Readiness<kqueue::Kqueue>),
    #[cfg(windows)]
    Iocp(iocp::Iocp),
}

/// Expands to a `match` on a [`Driver`] that runs the same expression for
/// every variant, binding the driver to `$d`.
macro_rules! each {
    ($self:expr, $d:ident => $body:expr) => {
        match $self {
            #[cfg(target_os = "linux")]
            Driver::Uring($d) => $body,
            #[cfg(target_os = "linux")]
            Driver::Epoll($d) => $body,
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            Driver::Kqueue($d) => $body,
            #[cfg(windows)]
            Driver::Iocp($d) => $body,
        }
    };
}

/// Like [`each`], but the io_uring driver runs `$uring` and the readiness
/// drivers run `$ready`.
macro_rules! split {
    ($self:expr, $d:ident => $uring:expr, $ready:expr) => {
        match $self {
            #[cfg(target_os = "linux")]
            Driver::Uring($d) => $uring,
            #[cfg(target_os = "linux")]
            Driver::Epoll($d) => {
                let _ = &$d;
                $ready
            }
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            Driver::Kqueue($d) => {
                let _ = &$d;
                $ready
            }
            #[cfg(windows)]
            Driver::Iocp($d) => {
                let _ = &$d;
                $ready
            }
        }
    };
}

/// Runs a non-blocking send for [`Driver::try_send_to`]: its result, or
/// `None` when the socket is full or the task's budget is spent.
fn send_now(mut send: impl FnMut() -> isize) -> Option<i32> {
    if !budget::spend_budget() {
        return None;
    }
    loop {
        let res = send();
        if res >= 0 {
            return Some(res.min(i32::MAX as isize) as i32);
        }
        match io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(crate::sys::errno::EIO)
        {
            crate::sys::errno::EINTR => {}
            crate::sys::errno::EAGAIN => {
                // Full: the operation the caller submits instead waits for
                // room, and takes its own unit of the budget.
                budget::give_back_budget();
                return None;
            }
            e => return Some(-e),
        }
    }
}

/// Reads from a regular file at once when the bytes are in the page cache:
/// `preadv2` with `RWF_NOWAIT`, which copies what is cached and returns
/// `EAGAIN` instead of waiting for the disk. A cached read then costs one
/// system call, as with blocking I/O, instead of a trip through io_uring (or
/// epoll's blocking threads). `None` when the caller must use the driver:
/// nothing is cached, the file system cannot do it, the call failed in any
/// other way (the driver's operation then reports the error as before), or
/// the task's budget is spent, so a loop over cached reads still yields.
#[cfg(unix)]
pub(crate) fn read_cached(fd: RawFile, buf: *mut u8, len: u32, offset: u64) -> Option<i32> {
    // Miri cannot run `preadv2`: every read goes through the driver there.
    if cfg!(miri) || len == 0 || offset > i64::MAX as u64 || !budget::spend_budget() {
        return None;
    }
    let iov = libc::iovec {
        iov_base: buf.cast(),
        iov_len: len as usize,
    };
    // SAFETY: the caller hands a buffer valid for `len` bytes; the call does
    // not block.
    let res = unsafe { crate::sys::pread_cached(fd, &iov, offset) };
    if res >= 0 {
        return Some(res as i32);
    }
    // The driver's operation takes its own unit of the budget.
    budget::give_back_budget();
    None
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
            "kqueue" => Ok(Some(DriverKind::Kqueue)),
            "iocp" => Ok(Some(DriverKind::Iocp)),
            other => Err(crate::error::raise(
                Code::InvalidConfig,
                format!("GIONG_DRIVER is {other:?}, which is not a driver"),
                "set GIONG_DRIVER to io_uring or epoll (Linux), kqueue (macOS) or iocp \
                 (Windows), or unset it to use io_uring where the kernel allows it, epoll \
                 elsewhere on Linux, kqueue on macOS and iocp on Windows.",
            )),
        },
    }
}

#[cfg(target_os = "linux")]
/// `true` if an io_uring setup error means "io_uring cannot be used here",
/// rather than a mistake in the configuration: the automatic choice then
/// falls back to epoll. That includes a used-up locked-memory limit, which
/// the user's other processes share: a worker that would otherwise fail to
/// start runs on epoll.
fn io_uring_unusable(e: &io::Error) -> bool {
    matches!(
        crate::error::code(e),
        Some(
            Code::IoUringUnavailable
                | Code::KernelTooOld
                | Code::BufferRingRefused
                | Code::LockedMemoryLimit
        )
    ) || matches!(
        e.raw_os_error(),
        Some(libc::EPERM | libc::EACCES | libc::ENOSYS | libc::EINVAL)
    )
}

#[cfg(target_os = "linux")]
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

/// Whether this build has the driver `kind`.
fn exists(kind: DriverKind) -> bool {
    match kind {
        DriverKind::IoUring | DriverKind::Epoll => cfg!(target_os = "linux"),
        DriverKind::Kqueue => cfg!(any(target_os = "macos", giong_libkqueue)),
        DriverKind::Iocp => cfg!(windows),
        #[allow(unreachable_patterns)]
        _ => false,
    }
}

/// Fails, as [`Driver::new`] would, for a driver that this operating system
/// does not have: so that `Builder::build` refuses it, before any worker
/// starts.
pub(crate) fn check_exists(kind: DriverKind) -> io::Result<()> {
    if exists(kind) {
        Ok(())
    } else {
        Err(unsupported(kind))
    }
}

/// The error for a driver this operating system does not have: io_uring is
/// not available (`GIONG-E0001`, as `Builder::driver` promises), and the
/// others are a wrong setting (`GIONG-E0004`).
fn unsupported(kind: DriverKind) -> io::Error {
    let code = match kind {
        DriverKind::IoUring => Code::IoUringUnavailable,
        _ => Code::InvalidConfig,
    };
    crate::error::raise(
        code,
        format!("the {kind} driver does not exist on this operating system"),
        "name io_uring or epoll on Linux, kqueue on macOS and iocp on Windows \
         (Builder::driver or GIONG_DRIVER), or name none to let giong choose.",
    )
}

/// Wakes a worker that sleeps in its driver, from any thread.
///
/// Each driver makes its own wake-up when it is created and hands this out
/// with [`Driver::waker`], one variant per mechanism: an eventfd read by the
/// ring or watched by epoll on Linux (a user event of the kqueue, or a
/// completion packet posted to the port, would be others). What every
/// variant keeps:
///
/// - it is `Send`, `Sync` and `Clone`, and [`ring`](Self::ring) may be
///   called from any thread, any number of times, without blocking;
/// - a ring that arrives while the worker is awake is not lost: the next
///   `park` returns at once;
/// - it may outlive the driver and its worker; ringing then does nothing.
#[derive(Clone)]
pub(crate) enum WakeHandle {
    /// A bell the driver watches: io_uring reads it, epoll polls it.
    #[cfg(target_os = "linux")]
    Bell(std::sync::Arc<crate::sys::Bell>),
    /// The user event of a kqueue.
    #[cfg(any(target_os = "macos", giong_libkqueue))]
    Kqueue(std::sync::Arc<kqueue::Wake>),
    /// A completion port: a packet posted to it wakes the worker that waits
    /// on it.
    #[cfg(windows)]
    Port(std::sync::Arc<iocp::Port>),
}

impl WakeHandle {
    /// Wakes the worker, or makes its next `park` return at once.
    pub(crate) fn ring(&self) {
        match self {
            #[cfg(target_os = "linux")]
            WakeHandle::Bell(bell) => bell.ring(),
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            WakeHandle::Kqueue(wake) => wake.ring(),
            #[cfg(windows)]
            WakeHandle::Port(port) => port.post(),
        }
    }

    /// Resets the wake-up once the driver saw it, where it does not reset
    /// itself.
    #[cfg_attr(windows, allow(dead_code))]
    fn seen(&self) {
        match self {
            #[cfg(target_os = "linux")]
            WakeHandle::Bell(bell) => bell.drain(),
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            WakeHandle::Kqueue(_) => {}
            // A packet is gone once it is dequeued.
            #[cfg(windows)]
            WakeHandle::Port(_) => {}
        }
    }
}

impl Driver {
    /// Sets up the driver `choice` names, or the platform's best when it is
    /// `None`: io_uring with epoll as the fallback on Linux, kqueue on
    /// macOS. `chosen_by` says where `choice` came from.
    pub(crate) fn new(
        choice: Option<DriverKind>,
        chosen_by: ChosenBy,
        entries: u32,
        pool: Rc<BufPool>,
    ) -> io::Result<(Self, DriverChoice)> {
        let _ = entries;
        #[allow(unused_mut)]
        let mut io_uring_error: Option<io::Error> = None;
        let driver = match choice {
            #[cfg(target_os = "linux")]
            Some(DriverKind::IoUring) => Driver::Uring(uring::Uring::new(entries, pool)?),
            #[cfg(target_os = "linux")]
            Some(DriverKind::Epoll) => Driver::Epoll(readiness::Readiness::new(pool)?),
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            Some(DriverKind::Kqueue) => Driver::Kqueue(readiness::Readiness::new(pool)?),
            #[cfg(windows)]
            Some(DriverKind::Iocp) | None => Driver::Iocp(iocp::Iocp::new(pool)?),
            // Reachable for the drivers this build does not have.
            #[allow(unreachable_patterns)]
            Some(other) => return Err(unsupported(other)),
            #[cfg(target_os = "linux")]
            None => match uring::Uring::new(entries, pool.clone()) {
                Ok(ring) => Driver::Uring(ring),
                Err(e) if io_uring_unusable(&e) => {
                    let driver = Driver::Epoll(readiness::Readiness::new(pool)?);
                    report_fallback(&e);
                    io_uring_error = Some(e);
                    driver
                }
                Err(e) => return Err(e),
            },
            #[cfg(target_os = "macos")]
            None => Driver::Kqueue(readiness::Readiness::new(pool)?),
        };
        let choice = DriverChoice {
            kind: driver.kind(),
            chosen_by,
            io_uring_error_code: io_uring_error.as_ref().and_then(crate::error::code),
            io_uring_error: io_uring_error.map(|e| e.to_string()),
            kernel_workarounds: split!(&driver, d => d.workarounds().to_vec(), Vec::new()),
            io_uring_features: split!(&driver, d => d.features().to_vec(), Vec::new()),
        };
        Ok((driver, choice))
    }

    /// Times short waits in [`park`](Self::park) exactly, without the
    /// kernel's timer slack: see [`Builder::precise_timers`]. Only kqueue
    /// has that slack; the other drivers ignore it.
    ///
    /// [`Builder::precise_timers`]: crate::Builder::precise_timers
    pub(crate) fn set_precise_timers(&mut self, enable: bool) {
        match self {
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            Driver::Kqueue(d) => d.set_precise_timers(enable),
            #[allow(unreachable_patterns)]
            _ => {
                let _ = enable;
            }
        }
    }

    /// The handle that wakes this driver's worker when it sleeps in
    /// [`park`](Self::park); see [`WakeHandle`] for what it promises.
    pub(crate) fn waker(&self) -> WakeHandle {
        each!(self, d => d.waker())
    }

    pub(crate) fn kind(&self) -> DriverKind {
        match self {
            #[cfg(target_os = "linux")]
            Driver::Uring(_) => DriverKind::IoUring,
            #[cfg(target_os = "linux")]
            Driver::Epoll(_) => DriverKind::Epoll,
            #[cfg(any(target_os = "macos", giong_libkqueue))]
            Driver::Kqueue(_) => DriverKind::Kqueue,
            #[cfg(windows)]
            Driver::Iocp(_) => DriverKind::Iocp,
        }
    }

    /// Starts a single-shot operation; `waker` is woken when it can be
    /// polled again.
    ///
    /// The deadline of the scope the task runs in, if there is one
    /// ([`crate::task`]), goes with an io_uring operation that waits on a
    /// peer ([`Io::links_deadline`]): io_uring links a kernel timeout to it,
    /// so the kernel cancels it at the deadline. The other drivers start
    /// every operation as usual: there the deadline's timer in the worker's
    /// timer wheel ends it.
    #[inline]
    pub(crate) fn start(&mut self, io: &Io, waker: &Waker) -> Start {
        split!(
            self,
            d => {
                let deadline = if d.links_deadlines() && io.links_deadline() {
                    crate::task::kernel_deadline()
                } else {
                    None
                };
                Start::Submitted(match deadline {
                    None => d.submit_op(io, waker),
                    Some(at) => d.submit_op_linked(io, waker, at),
                })
            },
            d.start(io, waker)
        )
    }

    /// Polls a single-shot operation, consuming its entry when it is done.
    /// A zero-copy send with a `hold` ([`ZeroCopyHold`]) is done once it has
    /// its result; the driver then takes the hold.
    pub(crate) fn poll_op(
        &mut self,
        key: usize,
        waker: &Waker,
        hold: &mut Option<Box<ZeroCopyHold>>,
    ) -> Poll<(i32, u32)> {
        split!(self, d => d.poll_op(key, waker, hold), {
            let _ = &hold;
            d.poll_op(key, waker)
        })
    }

    /// The future of a single-shot operation was dropped before its result
    /// was collected. `data` are the resources the operation may still use.
    /// The future of a single-shot operation was dropped before its result
    /// was collected. `data` are the resources the kernel may still use;
    /// `result_is_fd` says the result will be a descriptor nobody owns (see
    /// [`Io::result_is_fd`]), which the driver then closes. With `finish`
    /// (see [`Io::finishes_when_dropped`]) the operation is not cancelled:
    /// it runs to the end and its result is thrown away.
    pub(crate) fn cancel_op(
        &mut self,
        key: usize,
        data: Option<Box<dyn Any>>,
        result_is_fd: bool,
        finish: bool,
    ) {
        each!(self, d => d.cancel_op(key, data, result_is_fd, finish))
    }

    pub(crate) fn new_multi(&mut self, kind: MultiKind) -> usize {
        each!(self, d => d.new_multi(kind))
    }

    /// Sends one datagram to `addr` at once with `sendto(2)`, without
    /// blocking: the result, or `None` if the caller is to send it as an
    /// operation instead, because the socket buffer is full or the task has
    /// used its budget (see [`refill_budget`]).
    ///
    /// Both drivers send datagrams this way first. A UDP send never waits for
    /// the peer, so the kernel finishes it within the call. With io_uring it
    /// is cheaper than an SQE: the datagram leaves at once instead of at the
    /// next `io_uring_enter`, and the task carries on without waiting for a
    /// completion, which with one task per socket meant a turn of the event
    /// loop per datagram. Receives stay multishot, so an echo costs one
    /// system call, where a readiness runtime makes two.
    ///
    /// Not while SQEs wait for the next `io_uring_enter`: the datagram would
    /// reach the kernel before them. One of them may cancel a receive on this
    /// socket, and on kernels that run completions before taking new
    /// submissions (Linux 6.0) the answer to this datagram would go to the
    /// receive being cancelled and be lost. The operation the caller submits
    /// instead goes in behind them.
    #[inline(always)]
    pub(crate) fn try_send_to(
        &mut self,
        fd: RawSock,
        buf: &[u8],
        addr: &socket2::SockAddr,
    ) -> Option<i32> {
        if self.has_queued_submissions() {
            return None;
        }
        // SAFETY: `buf` and `addr` are valid for the call; non-blocking.
        send_now(|| unsafe {
            crate::sys::send_to_nowait(
                fd,
                buf.as_ptr(),
                buf.len(),
                addr.as_ptr().cast(),
                addr.len(),
            )
        })
    }

    /// [`try_send_to`](Self::try_send_to) for a connected socket: `send(2)`.
    #[inline(always)]
    pub(crate) fn try_send(&mut self, fd: RawSock, buf: &[u8]) -> Option<i32> {
        if self.has_queued_submissions() {
            return None;
        }
        // SAFETY: `buf` is valid for the call; non-blocking.
        send_now(|| unsafe { crate::sys::send_nowait(fd, buf.as_ptr(), buf.len()) })
    }

    /// `true` if [`Io::SendTo`] can be submitted: always with epoll, from
    /// Linux 6.1 on with io_uring. Without it, a datagram to an address goes
    /// as an [`Io::SendMsg`].
    pub(crate) fn sends_to(&self) -> bool {
        split!(self, d => d.sends_to(), true)
    }

    pub(crate) fn arm_multi(&mut self, key: usize, io: MultiIo) {
        each!(self, d => d.arm_multi(key, io))
    }

    pub(crate) fn poll_multi(&mut self, key: usize, waker: &Waker) -> MultiPoll {
        each!(self, d => d.poll_multi(key, waker))
    }

    /// Lets a multishot operation run out (see `Iocp::finish_multi`).
    #[cfg(windows)]
    pub(crate) fn finish_multi(&mut self, key: usize) {
        each!(self, d => d.finish_multi(key))
    }

    /// Ends the receives that wait on stream `fd`, whose reading side was
    /// shut (see `Iocp::end_reads`).
    #[cfg(windows)]
    pub(crate) fn end_reads(&mut self, fd: RawSock) {
        each!(self, d => d.end_reads(fd))
    }

    /// Takes a connection an accept has completed with and nobody has
    /// collected yet (see `Iocp::take_accepted`).
    #[cfg(windows)]
    pub(crate) fn take_accepted(&mut self, key: usize) -> Option<i32> {
        each!(self, d => d.take_accepted(key))
    }

    /// Stops a multishot receive that is waiting in the kernel, so that the
    /// reader's next poll starts it again (io_uring; the epoll driver reads
    /// when the socket is readable and misses nothing).
    pub(crate) fn restart_multi(&mut self, key: usize) {
        split!(self, d => d.restart_multi(key), {
            let _ = key;
        })
    }

    /// [`try_send_to`](Self::try_send_to) for a prepared message (control
    /// data such as `UDP_SEGMENT`): `sendmsg(2)`.
    #[cfg(feature = "unstable-udp-offload")]
    pub(crate) fn try_sendmsg(&mut self, fd: RawSock, msg: *const MsgHdr) -> Option<i32> {
        if self.has_queued_submissions() {
            return None;
        }
        // SAFETY: the caller's header points at live buffers.
        send_now(|| unsafe { crate::sys::sendmsg_nowait(fd, msg) })
    }

    /// A future that waited on multishot operation `key` with `waker` was
    /// dropped (see `net::Waiting`).
    pub(crate) fn leave_multi(&mut self, key: usize, waker: &Waker) {
        // A readiness edge wakes every waiting task.
        split!(self, d => d.leave_multi(key, waker), {
            let _ = (key, waker);
        })
    }

    /// The owner of a multishot operation (listener, stream) goes away.
    pub(crate) fn drop_multi(&mut self, key: usize) {
        each!(self, d => d.drop_multi(key))
    }

    /// Moves received-but-unread data out of the buffer pool (io_uring; the
    /// epoll driver never queues pool buffers).
    pub(crate) fn evacuate(&mut self) {
        #[cfg(target_os = "linux")]
        if let Driver::Uring(d) = self {
            d.evacuate();
        }
    }

    /// Keeps `fd` off epoll: its reads and writes go to a blocking thread,
    /// so the driver never makes it non-blocking. For descriptors whose
    /// open file the process shares with others (its own stdin, stdout and
    /// stderr), where `O_NONBLOCK` would reach them too. io_uring never
    /// changes a descriptor's flags, so it has nothing to do.
    #[cfg(target_os = "linux")]
    pub(crate) fn keep_blocking(&mut self, fd: RawFile) {
        split!(self, d => {
            let _ = d;
        }, d.keep_blocking(fd))
    }

    /// Forgets `fd`, which the caller hands on without closing it (to a
    /// child process, say): the epoll driver takes it out of its epoll set.
    #[cfg(target_os = "linux")]
    pub(crate) fn release_fd(&mut self, fd: RawFile) {
        split!(self, d => {
            let _ = d;
        }, d.release_fd(fd))
    }

    /// Closes a descriptor the caller gives up.
    pub(crate) fn close_fd(&mut self, fd: RawFile) {
        each!(self, d => d.close_fd(fd))
    }

    /// Closes a socket the caller gives up: a descriptor like any other on
    /// Unix, a handle of its own kind on Windows.
    pub(crate) fn close_sock(&mut self, fd: RawSock) {
        #[cfg(unix)]
        self.close_fd(fd);
        #[cfg(windows)]
        each!(self, d => d.close_sock(fd))
    }

    /// `true` if operations wait to be handed to the kernel (io_uring's
    /// submission queue); never with epoll, which has no queue.
    #[inline(always)]
    pub(crate) fn has_queued_submissions(&mut self) -> bool {
        split!(self, d => d.has_queued_submissions(), false)
    }

    /// Hands queued SQEs to the kernel now (see `Uring::submit_queued`).
    pub(crate) fn submit_queued(&mut self) {
        split!(self, d => d.submit_queued(), {})
    }

    pub(crate) fn pool(&self) -> &Rc<BufPool> {
        each!(self, d => d.pool())
    }

    /// Makes sure a receive of this kind can get a pool buffer before it is
    /// armed (`whole`: a receive of datagrams, or a stream receive of one
    /// buffer; else a multishot stream receive). `false` if none is free.
    pub(crate) fn ensure_recv_buffer(&self, whole: bool) -> bool {
        split!(self, d => d.ensure_recv_buffer(whole), {
            let _ = whole;
            d.pool().ensure_published(None)
        })
    }

    /// The smallest send that goes out without a copy, or `None` where
    /// sends are always copied (epoll, or io_uring without
    /// [`IoUringFeature::ZeroCopySend`]).
    pub(crate) fn zero_copy_min(&self) -> Option<usize> {
        split!(self, d => d.zero_copy_min(), None)
    }

    /// `true` if there is something to collect without waiting.
    pub(crate) fn has_completions(&mut self) -> bool {
        each!(self, d => d.has_completions())
    }

    /// Enters the kernel, then wakes the tasks whose operations can make
    /// progress. Returns resources that must be dropped outside the driver.
    pub(crate) fn park(&mut self, mode: Park) -> Vec<Box<dyn Any>> {
        each!(self, d => d.park(mode))
    }

    /// Cancels what is left and waits for it. `false` means something may
    /// still use the driver's memory: the caller must leak it.
    pub(crate) fn shutdown(&mut self) -> bool {
        each!(self, d => d.shutdown())
    }

    pub(crate) fn take_graveyard(&mut self) -> Vec<Box<dyn Any>> {
        each!(self, d => d.take_graveyard())
    }

    /// Operations whose final completion is still outstanding.
    pub(crate) fn inflight(&self) -> usize {
        each!(self, d => d.inflight())
    }

    /// Number of tracked operations (used by tests to detect leaks).
    pub(crate) fn tracked_ops(&self) -> usize {
        each!(self, d => d.tracked_ops())
    }

    /// Received bytes moved from pool buffers to the heap so far, because
    /// their reader fell behind while the pool ran low, or the pool ran out.
    /// The epoll driver reads only when asked, so it never does.
    pub(crate) fn recv_copied(&self) -> u64 {
        split!(self, d => d.recv_copied(), 0)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    /// The panic that stops a worker says what happened and what to do.
    #[test]
    fn a_fatal_event_loop_error_panics_with_its_code() {
        let message = |errno| {
            let err = std::panic::catch_unwind(|| {
                super::event_loop_failed("io_uring_enter", io::Error::from_raw_os_error(errno))
            })
            .unwrap_err();
            err.downcast_ref::<String>().cloned().unwrap()
        };
        #[cfg(target_os = "linux")]
        {
            let lost = message(libc::EBADR);
            assert!(
                lost.starts_with("GIONG-E0005: io_uring_enter failed"),
                "{lost}"
            );
            assert!(lost.contains("ran out of memory"), "{lost}");
        }
        #[cfg(unix)]
        {
            let oom = message(libc::ENOMEM);
            assert!(oom.contains("out of memory"), "{oom}");
        }
        let other = message(22);
        assert!(other.contains("report it"), "{other}");
    }
}
