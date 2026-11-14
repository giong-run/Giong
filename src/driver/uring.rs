//! The io_uring driver: one ring per worker thread.
//!
//! Every submitted operation is tracked in a slab; the slab key travels to the
//! kernel as the SQE's `user_data` and comes back on the CQE.
//!
//! ## Cancellation safety
//!
//! io_uring is completion based. Once an SQE is submitted, the kernel may use
//! the buffers it points at until the matching CQE is produced — no matter
//! what happens to the Rust future that started it. So when an operation's
//! future is dropped early we:
//!
//! 1. move the operation's resources (its buffer) into the slab entry
//!    ([`Lifecycle::Ignored`]) so they stay alive,
//! 2. ask the kernel to cancel the operation, and
//! 3. free the resources only when the final CQE arrives.
//!
//! Multishot operations (accept, recv) own no user buffers; when their owner
//! goes away they are marked `orphan` and anything they still produce (accepted
//! sockets, provided buffers) is released by the driver itself.

use std::any::Any;
use std::collections::VecDeque;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, RawFd};
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use io_uring::{cqueue, opcode, squeue, types, IoUring};

use super::{
    Io, IoUringFeature, KernelWorkaround, MultiIo, MultiKind, MultiPoll, Park, ZeroCopyHold,
    F_BUF_MORE, ZC_COPIED,
};
use crate::buf::{BufPool, Received, RecvBuf, RingId};
use crate::error::{Code, Error};
use crate::slab::Slab;

/// `user_data` of operations whose completion nobody waits for, such as cancels.
const UD_IGNORE: u64 = u64::MAX;
/// `user_data` of the eventfd read used for cross-thread wake-ups.
const UD_WAKE: u64 = u64::MAX - 1;
/// `user_data` of batched closes, which must finish before shutdown cancels I/O.
const UD_CLOSE: u64 = u64::MAX - 2;

/// Backpressure for multishot receive. Once a stream has this many
/// completions its reader has not taken yet:
///
/// * its multishot receive is paused (cancelled; re-armed when the queue has
///   been drained), which pushes back on the peer through TCP flow control,
///   and
/// * while the pool runs low (under a quarter of its buffers free), what
///   still arrives for it is copied to the heap so that its pool buffer can
///   be recycled at once.
///
/// The limit is an eighth of the pool, and at least 8: a busy stream fills
/// dozens of buffers between two turns of the event loop, and a reader that
/// keeps up should get them all without copies or pauses. (A limit of 8 for
/// every pool copied most of a bulk transfer out of the pool.) Together with
/// [`Uring::evacuate`] this guarantees that slow or stalled connections
/// cannot pin the shared buffer pool.
fn recv_backlog_limit(pool: &BufPool) -> usize {
    (pool.count() as usize / 8).max(8)
}

/// A multishot accept is paused once this many accepted connections are
/// waiting for the application; further connections then wait in the
/// kernel's listen backlog instead of piling up as open descriptors.
const ACCEPT_BACKLOG_LIMIT: usize = 1024;

enum Lifecycle {
    /// In flight; the waker of the task awaiting it.
    Waiting(Waker),
    /// A zero-copy send has its result, but the kernel may still read the
    /// buffer until its notification arrives. Its future gets the result
    /// now if it leaves a [`ZeroCopyHold`], else at the notification.
    Sent(Waker, i32, u32),
    /// Finished; the result has not been collected yet.
    Done(i32, u32),
    /// The future was dropped while in flight. Holds the operation's resources
    /// until the kernel is done with them.
    Ignored(Option<Box<dyn Any>>),
}

/// A multishot completion waiting to be consumed.
enum Queued {
    /// The result as the kernel reported it, for completions without data:
    /// an accepted descriptor, the end of a stream, an error.
    Raw(i32),
    /// Received data in a pool buffer, held as one piece (see
    /// [`BufPool::received`]), and the event loop turn it arrived in.
    Piece {
        bid: u16,
        off: u32,
        len: u32,
        turn: u32,
    },
    /// Received data that was moved out of the pool.
    Heap(Vec<u8>),
}

struct Multi {
    kind: MultiKind,
    /// The buffer ring a receive takes its buffers from.
    ring: RingId,
    /// Completions not yet consumed.
    queue: VecDeque<Queued>,
    waker: Option<Waker>,
    /// An SQE is in flight and may still produce completions.
    armed: bool,
    /// We asked the kernel to stop (backpressure); the terminal CQE is pending.
    pausing: bool,
    /// The owner is gone; release whatever still arrives.
    orphan: bool,
}

enum OpState {
    Single(Lifecycle),
    Multi(Multi),
}

pub(crate) struct Uring {
    ring: IoUring,
    /// Kernel bugs worked around while setting the ring up.
    workarounds: Vec<KernelWorkaround>,
    ops: Slab<OpState>,
    /// SQEs pushed whose final CQE has not been seen yet.
    inflight: usize,
    /// Batched closes whose CQE has not been seen yet.
    pending_closes: usize,
    pool: Rc<BufPool>,
    /// Scratch space for a batch of CQEs.
    cqes: Vec<(u64, i32, u32)>,
    /// Multishot receives to pause once the current batch is dispatched.
    to_pause: Vec<usize>,
    /// A receive ran out of pool buffers during the current batch.
    starved: bool,
    /// Queued receive completions that hold a piece of a pool buffer.
    pooled_queued: usize,
    /// [`recv_backlog_limit`] of the pool.
    recv_limit: usize,
    /// Received bytes moved out of the pool to the heap.
    recv_copied: u64,
    /// Counts the event loop's turns (calls to `park`), wrapping. Tasks run
    /// between two turns, so data that arrived in an earlier turn than the
    /// current one has been in front of its reader at least once.
    turn: u32,
    /// io_uring features this ring uses.
    features: Vec<IoUringFeature>,
    /// The ring's index in this thread's registered ring table, with
    /// [`IoUringFeature::RegisteredRing`].
    registered_ring: Option<u32>,
    /// The smallest send that goes out without a copy, with
    /// [`IoUringFeature::ZeroCopySend`].
    zero_copy_min: Option<usize>,
    /// Resources of cancelled operations, handed to the event loop to drop
    /// outside of the driver borrow.
    graveyard: Vec<Box<dyn Any>>,
    wake_fd: RawFd,
    wake_buf: Box<u64>,
    wake_armed: bool,
    closing: bool,
}

/// The io_uring setup tiers, best first. `build_ring` starts at the first tier
/// the kernel accepts.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum RingTier {
    /// Kernel >= 6.1: `SINGLE_ISSUER` + `DEFER_TASKRUN`.
    Defer,
    /// Kernel 5.19 / 6.0: `COOP_TASKRUN`.
    Coop,
    /// No task-run flags at all.
    Basic,
}

/// Test hook: `GIONG_RING_TIER=coop|basic` skips the better tiers, so the
/// fallback paths meant for older kernels can be exercised on a new one
/// (`scripts/check.sh fallback`). Unset or unrecognised means "best available".
fn first_tier() -> RingTier {
    match std::env::var("GIONG_RING_TIER").as_deref() {
        Ok("coop") => RingTier::Coop,
        Ok("basic") => RingTier::Basic,
        _ => RingTier::Defer,
    }
}

fn build_ring(entries: u32) -> io::Result<IoUring> {
    build_ring_from(entries, first_tier())
        .map_err(explain_setup_error)
        .map_err(|e| explain_out_of_memory(e, "rings"))
}

/// `io_uring_setup` fails with `EPERM` when a seccomp profile blocks it (as
/// Docker's default profile does) or the `kernel.io_uring_disabled` sysctl
/// forbids it, and with `ENOSYS` where the kernel or a sandbox such as gVisor
/// does not provide it. The bare errno says none of that, so say it here.
fn explain_setup_error(e: io::Error) -> io::Error {
    match e.raw_os_error() {
        // The kernel's kind (PermissionDenied or Unsupported) is kept.
        Some(libc::EPERM | libc::EACCES | libc::ENOSYS) => io::Error::new(
            e.kind(),
            Error::new(
                Code::IoUringUnavailable,
                format!("giong needs io_uring, which is not available here ({e})"),
                "allow io_uring for this process, or let giong use its epoll driver by not \
                 asking for io_uring (Builder::driver, GIONG_DRIVER). Docker's default seccomp \
                 profile blocks io_uring, sandboxes such as gVisor do not have it, and the \
                 kernel.io_uring_disabled sysctl turns it off; docs/DEPLOYMENT.md shows how to \
                 allow it in Docker and Kubernetes.",
            )
            .with_source(e),
        ),
        _ => e,
    }
}

/// Linux 6.14 to 7.2 count the memory of every io_uring ring (the
/// submission and completion queues, and each buffer ring) against the
/// locked-memory limit, `RLIMIT_MEMLOCK`, summed over all of the user's
/// processes, unless the process has `CAP_IPC_LOCK`; 7.3 stopped, and so
/// did 6.18 from 6.18.58. Once the limit is used up, setting up a ring
/// fails with a bare `ENOMEM`, which reads as "out of memory" on a machine
/// with plenty of it; with a default limit of 8 MiB that happens at about
/// fifty workers. Say what it is here.
///
/// `what` names the memory: "rings" or "buffer ring". Other errors, and an
/// `ENOMEM` where no such limit applies, pass through.
fn explain_out_of_memory(e: io::Error, what: &str) -> io::Error {
    if e.raw_os_error() != Some(libc::ENOMEM) {
        return e;
    }
    match locked_memory_limit() {
        Some(limit) => locked_memory_error(e, what, limit),
        None => e,
    }
}

/// The locked-memory limit that io_uring's memory counts against, in bytes:
/// the soft `RLIMIT_MEMLOCK`, unless it is unlimited or this thread has
/// `CAP_IPC_LOCK`, which lifts it.
fn locked_memory_limit() -> Option<u64> {
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit` fills the struct it is given, which is of its type.
    if unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) } != 0
        || lim.rlim_cur == libc::RLIM_INFINITY
        || may_lock_any_memory()
    {
        return None;
    }
    Some(lim.rlim_cur)
}

/// `true` if this thread has `CAP_IPC_LOCK` in the initial user namespace,
/// which is where the kernel looks: root in a user namespace of its own (a
/// rootless container) has it only there, and is held to the limit.
fn may_lock_any_memory() -> bool {
    const CAP_IPC_LOCK: u32 = 14;
    let initial_namespace = std::fs::read_to_string("/proc/self/uid_map")
        .is_ok_and(|m| m.split_whitespace().eq(["0", "0", "4294967295"]));
    let capable = std::fs::read_to_string("/proc/thread-self/status").is_ok_and(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("CapEff:"))
            .and_then(|caps| u64::from_str_radix(caps.trim(), 16).ok())
            .is_some_and(|caps| caps & (1 << CAP_IPC_LOCK) != 0)
    });
    initial_namespace && capable
}

/// The error for an `ENOMEM` that the locked-memory limit, `limit` bytes,
/// is the likely cause of.
fn locked_memory_error(e: io::Error, what: &str, limit: u64) -> io::Error {
    io::Error::new(
        e.kind(),
        Error::new(
            Code::LockedMemoryLimit,
            format!(
                "io_uring could not lock memory for its {what}, because the locked-memory \
                 limit (RLIMIT_MEMLOCK, {}) is used up ({e})",
                bytes(limit)
            ),
            "raise the limit for this service: LimitMEMLOCK=infinity in its systemd unit, \
             --ulimit memlock=-1 with docker run, ulimit -l unlimited in a shell, or give it \
             CAP_IPC_LOCK, which lifts the limit. Linux 6.14 to 7.2 count every io_uring \
             against it, summed over all processes of the same user: a giong worker locks \
             about 150 KiB with the default settings, more with a larger \
             Builder::ring_entries or Builder::recv_buffers. Without \
             .driver(DriverKind::IoUring) or GIONG_DRIVER=io_uring, giong runs such a worker \
             on its epoll driver instead; docs/DEPLOYMENT.md shows how to raise the limit.",
        )
        .with_source(e),
    )
}

/// `n` bytes as people write them: `8 MiB`, `64 KiB`, `1000 bytes`.
fn bytes(n: u64) -> String {
    const KIB: u64 = 1 << 10;
    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    match n {
        0 => "0 bytes".into(),
        n if n % GIB == 0 => format!("{} GiB", n / GIB),
        n if n % MIB == 0 => format!("{} MiB", n / MIB),
        n if n % KIB == 0 => format!("{} KiB", n / KIB),
        n => format!("{n} bytes"),
    }
}

fn build_ring_from(entries: u32, first: RingTier) -> io::Result<IoUring> {
    let cq = entries.saturating_mul(4);

    // Preferred (kernel >= 6.1): completions are only processed when *we* enter
    // the kernel, on this thread — no IPIs, no surprise task work, best batching.
    if first <= RingTier::Defer {
        let mut b = IoUring::builder();
        b.setup_single_issuer()
            .setup_defer_taskrun()
            .setup_taskrun_flag()
            .setup_submit_all()
            .setup_cqsize(cq)
            .setup_clamp();
        if let Ok(ring) = b.build(entries) {
            return Ok(ring);
        }
    }

    // Kernel 5.19 / 6.0.
    if first <= RingTier::Coop {
        let mut b = IoUring::builder();
        b.setup_coop_taskrun()
            .setup_taskrun_flag()
            .setup_submit_all()
            .setup_cqsize(cq)
            .setup_clamp();
        if let Ok(ring) = b.build(entries) {
            return Ok(ring);
        }
    }

    let mut b = IoUring::builder();
    b.setup_cqsize(cq).setup_clamp();
    b.build(entries)
}

/// `IORING_CQE_F_MORE`: more completions of this operation follow.
const F_MORE: u32 = 1 << 1;
/// `IORING_CQE_F_NOTIF`: the notification of a zero-copy send, which says
/// the kernel no longer reads its buffer.
const F_NOTIF: u32 = 1 << 3;
/// `IORING_SEND_ZC_REPORT_USAGE`: the notification says whether the kernel
/// copied the data after all, in [`NOTIF_COPIED`] (Linux 6.2).
const SEND_ZC_REPORT_USAGE: u16 = 1 << 3;
/// `IORING_NOTIF_USAGE_ZC_COPIED`, in the result of a notification.
const NOTIF_COPIED: u32 = 1 << 31;

/// The SQE for a single-shot operation. `zero_copy` says whether the ring
/// uses [`IoUringFeature::ZeroCopySend`].
fn sqe_for(io: &Io, zero_copy: bool) -> squeue::Entry {
    let cwd = types::Fd(libc::AT_FDCWD);
    match *io {
        Io::Connect { fd, addr, len } => opcode::Connect::new(types::Fd(fd), addr, len).build(),
        Io::Recv { fd, buf, len } => opcode::Recv::new(types::Fd(fd), buf, len).build(),
        Io::Send {
            fd,
            buf,
            len,
            flags,
            zero_copy: true,
        } if zero_copy => opcode::SendZc::new(types::Fd(fd), buf, len)
            .flags(flags)
            .zc_flags(SEND_ZC_REPORT_USAGE)
            .build(),
        Io::Send {
            fd,
            buf,
            len,
            flags,
            ..
        } => opcode::Send::new(types::Fd(fd), buf, len)
            .flags(flags)
            .build(),
        Io::Writable { fd } => opcode::PollAdd::new(types::Fd(fd), libc::POLLOUT as u32).build(),
        Io::SendMsg {
            fd,
            msg,
            flags,
            zero_copy: true,
        } if zero_copy => opcode::SendMsgZc::new(types::Fd(fd), msg)
            .flags(flags)
            .ioprio(SEND_ZC_REPORT_USAGE)
            .build(),
        Io::SendMsg { fd, msg, flags, .. } => opcode::SendMsg::new(types::Fd(fd), msg)
            .flags(flags)
            .build(),
        Io::RecvMsg { fd, msg } => opcode::RecvMsg::new(types::Fd(fd), msg).build(),
        Io::ReadReady { fd, buf, len } => opcode::Read::new(types::Fd(fd), buf, len).build(),
        Io::Read {
            fd,
            buf,
            len,
            offset,
        } => opcode::Read::new(types::Fd(fd), buf, len)
            .offset(offset)
            .build(),
        Io::Write {
            fd,
            buf,
            len,
            offset,
        } => opcode::Write::new(types::Fd(fd), buf, len)
            .offset(offset)
            .build(),
        Io::OpenAt { path, flags, mode } => opcode::OpenAt::new(cwd, path)
            .flags(flags)
            .mode(mode)
            .build(),
        Io::Close { fd } => opcode::Close::new(types::Fd(fd)).build(),
        Io::Statx {
            dir,
            path,
            flags,
            mask,
            out,
        } => opcode::Statx::new(types::Fd(dir), path, out.cast())
            .flags(flags)
            .mask(mask)
            .build(),
        Io::Fsync { fd, datasync } => {
            let flags = if datasync {
                types::FsyncFlags::DATASYNC
            } else {
                types::FsyncFlags::empty()
            };
            opcode::Fsync::new(types::Fd(fd)).flags(flags).build()
        }
        Io::Fallocate { fd, offset, len } => opcode::Fallocate::new(types::Fd(fd), len)
            .offset(offset)
            .build(),
        Io::UnlinkAt { path, flags } => opcode::UnlinkAt::new(cwd, path).flags(flags).build(),
        Io::MkDirAt { path, mode } => opcode::MkDirAt::new(cwd, path).mode(mode).build(),
        Io::RenameAt { from, to } => opcode::RenameAt::new(cwd, from, cwd, to).build(),
    }
}

/// `IORING_REGISTER_PBUF_RING` and `IORING_UNREGISTER_PBUF_RING`.
const REGISTER_PBUF_RING: libc::c_uint = 22;
const UNREGISTER_PBUF_RING: libc::c_uint = 23;

/// `struct io_uring_buf_reg`, the argument of both. The `io-uring` crate
/// always zeroes `resv`, which the kernels in [`registers_with_reserved_bits`]
/// refuse, so giong fills it in itself.
#[repr(C)]
#[derive(Default)]
struct BufReg {
    ring_addr: u64,
    ring_entries: u32,
    bgid: u16,
    flags: u16,
    resv: [u64; 3],
}

const _: () = assert!(mem::size_of::<BufReg>() == 40);

/// `io_uring_register(2)` with a [`BufReg`].
fn register_pbuf(ring_fd: RawFd, opcode: libc::c_uint, reg: &BufReg) -> io::Result<()> {
    // SAFETY: `reg` is a valid `io_uring_buf_reg`, which the kernel copies
    // before the call returns; one argument, as both opcodes require.
    let res = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            ring_fd,
            opcode,
            reg as *const BufReg,
            1,
        )
    };
    if res < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `true` if this kernel refuses buffer-ring requests whose reserved fields
/// are zero and accepts them when they are not: the inverse of what every
/// correct kernel does (see [`KernelWorkaround::BufferRingReservedCheck`]).
///
/// It asks the kernel to unregister a buffer group that does not exist,
/// once with zeroed reserved fields and once with one set. A correct kernel
/// answers `ENOENT` to the first (nothing to unregister), and kernels without
/// buffer rings answer `EINVAL` to both. Only a kernel with the inverted
/// check answers `EINVAL`, then `ENOENT`. Neither call changes anything.
fn registers_with_reserved_bits(ring_fd: RawFd) -> bool {
    let probe = |resv: u64| {
        let reg = BufReg {
            // Not registered: giong uses groups 0 and 1 only, and
            // registering group 0 has just failed.
            bgid: u16::MAX,
            resv: [resv, 0, 0],
            ..BufReg::default()
        };
        register_pbuf(ring_fd, UNREGISTER_PBUF_RING, &reg)
            .err()
            .and_then(|e| e.raw_os_error())
    };
    probe(0) == Some(libc::EINVAL) && probe(1) == Some(libc::ENOENT)
}

/// Registers the pool's whole-buffer ring as buffer group 0, working around
/// the kernel bugs that stand in the way. Returns the workarounds it needed.
///
/// # Safety
///
/// The pool's ring memory must stay alive, at the same address, for as long
/// as `ring`: the kernel keeps it pinned and reads it.
unsafe fn register_buf_ring(ring: &IoUring, pool: &BufPool) -> io::Result<Vec<KernelWorkaround>> {
    let fd = ring.as_raw_fd();
    let mut reg = BufReg {
        ring_addr: pool.ring_addr(RingId::Whole),
        ring_entries: u32::from(pool.count()),
        bgid: RingId::Whole.group(),
        ..BufReg::default()
    };
    let err = match register_pbuf(fd, REGISTER_PBUF_RING, &reg) {
        Ok(()) => return Ok(Vec::new()),
        Err(e) => e,
    };
    if err.raw_os_error() == Some(libc::EINVAL) && registers_with_reserved_bits(fd) {
        // The reserved fields mean nothing to these kernels beyond the
        // inverted check; any non-zero value gets past it.
        reg.resv[0] = 1;
        if register_pbuf(fd, REGISTER_PBUF_RING, &reg).is_ok() {
            return Ok(vec![KernelWorkaround::BufferRingReservedCheck]);
        }
    }
    let err = explain_out_of_memory(err, "buffer ring");
    if crate::error::code(&err).is_some() {
        return Err(err);
    }
    Err(buffer_ring_error(&kernel_release(), err))
}

/// `IOU_PBUF_RING_INC`: the kernel fills ring buffers piece by piece
/// (Linux 6.12).
const PBUF_RING_INC: u16 = 2;

/// Registers the pool's packed ring as buffer group 1 if the kernel can fill
/// buffers piece by piece (Linux 6.12 and newer). `false` if it cannot, and
/// streams then share the whole-buffer ring with datagrams.
///
/// # Safety
///
/// As for [`register_buf_ring`].
unsafe fn register_packed_ring(
    ring: &IoUring,
    pool: &BufPool,
    workarounds: &[KernelWorkaround],
) -> bool {
    let mut reg = BufReg {
        ring_addr: pool.ring_addr(RingId::Packed),
        ring_entries: u32::from(pool.count()),
        bgid: RingId::Packed.group(),
        flags: PBUF_RING_INC,
        ..BufReg::default()
    };
    if workarounds.contains(&KernelWorkaround::BufferRingReservedCheck) {
        reg.resv[0] = 1;
    }
    // Older kernels refuse the flag with EINVAL.
    register_pbuf(ring.as_raw_fd(), REGISTER_PBUF_RING, &reg).is_ok()
}

/// `true` if the kernel sends without a copy the way giong asks it to: with
/// `IORING_OP_SEND_ZC` and `IORING_OP_SENDMSG_ZC`, whose notifications say
/// whether the data was copied after all (`IORING_SEND_ZC_REPORT_USAGE`,
/// Linux 6.2; a probe cannot see flags, hence the version).
fn has_zero_copy_send(ring: &IoUring) -> bool {
    if !kernel_version(&kernel_release()).is_some_and(|v| v >= (6, 2)) {
        return false;
    }
    let mut probe = io_uring::Probe::new();
    if ring.submitter().register_probe(&mut probe).is_err() {
        return false;
    }
    probe.is_supported(opcode::SendZc::CODE) && probe.is_supported(opcode::SendMsgZc::CODE)
}

/// `IORING_REGISTER_RING_FDS` and `IORING_UNREGISTER_RING_FDS`.
const REGISTER_RING_FDS: libc::c_uint = 20;
const UNREGISTER_RING_FDS: libc::c_uint = 21;
/// `IORING_ENTER_GETEVENTS`, `IORING_ENTER_EXT_ARG` and
/// `IORING_ENTER_REGISTERED_RING`.
const ENTER_GETEVENTS: u32 = 1 << 0;
const ENTER_EXT_ARG: u32 = 1 << 3;
const ENTER_REGISTERED_RING: u32 = 1 << 4;

/// `struct io_uring_rsrc_update`, the argument of both.
#[repr(C)]
struct RsrcUpdate {
    offset: u32,
    resv: u32,
    data: u64,
}

/// `struct io_uring_getevents_arg`, for a wait with a timeout.
#[repr(C)]
struct GeteventsArg {
    sigmask: u64,
    sigmask_sz: u32,
    min_wait_usec: u32,
    ts: u64,
}

/// `struct __kernel_timespec`.
#[repr(C)]
struct KernelTimespec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Puts `ring` in this thread's registered ring table, returning its index
/// there, or `None` where the kernel cannot (before 5.18, or the table is
/// full: it holds 16 rings per thread).
///
/// The table belongs to the thread, so the ring must be entered from this
/// thread only (which `SINGLE_ISSUER` asks anyway) and taken out of it on
/// this thread ([`unregister_ring_fd`]).
fn register_ring_fd(ring: &IoUring) -> Option<u32> {
    let mut up = RsrcUpdate {
        // Any free index.
        offset: u32::MAX,
        resv: 0,
        data: ring.as_raw_fd() as u64,
    };
    // SAFETY: the argument is one `io_uring_rsrc_update`, as the count says.
    let ret = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            ring.as_raw_fd(),
            REGISTER_RING_FDS,
            &mut up as *mut RsrcUpdate,
            1u32,
        )
    };
    (ret == 1).then_some(up.offset)
}

/// Takes the ring at `index` out of this thread's registered ring table.
fn unregister_ring_fd(ring: &IoUring, index: u32) {
    let mut up = RsrcUpdate {
        offset: index,
        resv: 0,
        data: 0,
    };
    // SAFETY: as in `register_ring_fd`.
    unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            ring.as_raw_fd(),
            UNREGISTER_RING_FDS,
            &mut up as *mut RsrcUpdate,
            1u32,
        )
    };
}

/// The running kernel's release (`uname -r`), or `"unknown"`.
fn kernel_release() -> String {
    // SAFETY: `utsname` is plain data, for which all zeroes is a valid value.
    let mut u: libc::utsname = unsafe { mem::zeroed() };
    // SAFETY: `uname` fills the struct it is given, which is of its type.
    if unsafe { libc::uname(&mut u) } != 0 {
        return "unknown".into();
    }
    // SAFETY: `release` is a NUL-terminated string inside `u`.
    unsafe { std::ffi::CStr::from_ptr(u.release.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

/// `(major, minor)` of a kernel release such as `6.8.0-147-generic`.
fn kernel_version(release: &str) -> Option<(u32, u32)> {
    let mut parts = release.split(|c: char| !c.is_ascii_digit());
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The error for a kernel that would not register a buffer ring, even with
/// the workarounds: too old if it is older than 6.0, refused otherwise.
fn buffer_ring_error(release: &str, e: io::Error) -> io::Error {
    let kind = e.kind();
    let old = kernel_version(release).is_some_and(|v| v < (6, 0));
    let err = if old {
        Error::new(
            Code::KernelTooOld,
            format!(
                "giong's io_uring driver needs buffer rings, from Linux 6.0, and this kernel \
                 is {release} ({e})"
            ),
            "run on Linux 6.0 or newer (6.1 or newer is faster), or let giong use its epoll \
             driver by not asking for io_uring (Builder::driver, GIONG_DRIVER).",
        )
    } else {
        Error::new(
            Code::BufferRingRefused,
            format!("this kernel ({release}) refused to register an io_uring buffer ring ({e})"),
            "let giong use its epoll driver by not asking for io_uring (Builder::driver, \
             GIONG_DRIVER). A kernel from 6.0 on should accept the ring, so this may be a \
             kernel bug giong does not know yet: please report it with the output of uname -r \
             at https://github.com/giong-run/Giong/issues.",
        )
    };
    io::Error::new(kind, err.with_source(e))
}

impl Uring {
    pub(crate) fn new(entries: u32, pool: Rc<BufPool>, wake_fd: RawFd) -> io::Result<Self> {
        let ring = build_ring(entries)?;
        // SAFETY: the ring memory is page aligned, holds `count` entries and is
        // kept alive by `pool`, which outlives the ring (see `Worker` teardown).
        let workarounds = unsafe { register_buf_ring(&ring, &pool)? };
        pool.set_registered(RingId::Whole);
        let mut features = Vec::new();
        // SAFETY: as above, for the pool's second ring.
        if super::feature_enabled(IoUringFeature::PackedBuffers)
            && unsafe { register_packed_ring(&ring, &pool, &workarounds) }
        {
            pool.set_registered(RingId::Packed);
            features.push(IoUringFeature::PackedBuffers);
        }
        let registered_ring = super::feature_enabled(IoUringFeature::RegisteredRing)
            .then(|| register_ring_fd(&ring))
            .flatten();
        if registered_ring.is_some() {
            features.push(IoUringFeature::RegisteredRing);
        }
        let zero_copy_min = (super::feature_enabled(IoUringFeature::ZeroCopySend)
            && has_zero_copy_send(&ring))
        .then(super::zero_copy_min);
        if zero_copy_min.is_some() {
            features.push(IoUringFeature::ZeroCopySend);
        }
        let recv_limit = recv_backlog_limit(&pool);
        Ok(Self {
            workarounds,
            features,
            registered_ring,
            zero_copy_min,
            ring,
            ops: Slab::new(),
            inflight: 0,
            pending_closes: 0,
            pool,
            cqes: Vec::with_capacity(256),
            to_pause: Vec::new(),
            starved: false,
            pooled_queued: 0,
            recv_limit,
            recv_copied: 0,
            turn: 0,
            graveyard: Vec::new(),
            wake_fd,
            wake_buf: Box::new(0),
            wake_armed: false,
            closing: false,
        })
    }

    // ---------------------------------------------------------------- submit

    /// Queues an SQE. The kernel sees it at the next `park`.
    fn push(&mut self, sqe: squeue::Entry) {
        let mut attempts = 0u32;
        loop {
            // SAFETY: callers guarantee that everything the SQE points at stays
            // valid until its completion (see the module docs).
            if unsafe { self.ring.submission().push(&sqe) }.is_ok() {
                break;
            }
            // Submission queue full: hand what we have to the kernel.
            match self.enter(0, None) {
                Ok(()) => {}
                Err(e) => match e.raw_os_error() {
                    Some(libc::EINTR) => {}
                    // Completion queue backed up: make room, then retry.
                    Some(libc::EBUSY) | Some(libc::EAGAIN) => self.reap(),
                    _ => panic!("giong: io_uring submit failed: {e}"),
                },
            }
            attempts += 1;
            assert!(
                attempts < 100_000,
                "giong: unable to make progress submitting to io_uring"
            );
        }
        self.inflight += 1;
    }

    /// Submits a single-shot operation; `waker` is woken on completion.
    pub(crate) fn submit_op(&mut self, io: &Io, waker: &Waker) -> usize {
        let key = self
            .ops
            .insert(OpState::Single(Lifecycle::Waiting(waker.clone())));
        let sqe = sqe_for(io, self.zero_copy_min.is_some());
        self.push(sqe.user_data(key as u64));
        key
    }

    /// Polls a single-shot operation, consuming its entry when it is done.
    /// A zero-copy send that has its result is done if the caller leaves its
    /// `hold`: the entry keeps that until the notification.
    pub(crate) fn poll_op(
        &mut self,
        key: usize,
        waker: &Waker,
        hold: &mut Option<Box<dyn Any>>,
    ) -> Poll<(i32, u32)> {
        match self.ops.get_mut(key) {
            Some(OpState::Single(Lifecycle::Done(res, flags))) => {
                let out = (*res, *flags);
                self.ops.remove(key);
                Poll::Ready(out)
            }
            Some(OpState::Single(lc @ Lifecycle::Sent(..))) if hold.is_some() => {
                let Lifecycle::Sent(_, res, flags) = *lc else {
                    unreachable!()
                };
                *lc = Lifecycle::Ignored(hold.take());
                Poll::Ready((res, flags))
            }
            Some(OpState::Single(Lifecycle::Waiting(w) | Lifecycle::Sent(w, ..))) => {
                if !w.will_wake(waker) {
                    *w = waker.clone();
                }
                Poll::Pending
            }
            _ => panic!("giong: operation polled in an invalid state"),
        }
    }

    /// Called when the future of a single-shot operation is dropped before its
    /// result was collected. `data` are the resources the kernel may still use.
    pub(crate) fn cancel_op(&mut self, key: usize, data: Option<Box<dyn Any>>) {
        match self.ops.get_mut(key) {
            Some(OpState::Single(lc @ Lifecycle::Waiting(_))) => {
                *lc = Lifecycle::Ignored(data);
                self.push(
                    opcode::AsyncCancel::new(key as u64)
                        .build()
                        .user_data(UD_IGNORE),
                );
            }
            Some(OpState::Single(lc @ Lifecycle::Sent(..))) => {
                // Sent: nothing to cancel, but the kernel may still read the
                // buffer until the notification.
                *lc = Lifecycle::Ignored(data);
            }
            Some(OpState::Single(Lifecycle::Done(..))) => {
                self.ops.remove(key);
                // Completed: the kernel no longer references the resources.
                if let Some(data) = data {
                    self.graveyard.push(data);
                }
            }
            _ => {
                // Unknown state: never free memory the kernel might still use.
                mem::forget(data);
            }
        }
    }

    // ------------------------------------------------------------- multishot

    pub(crate) fn new_multi(&mut self, kind: MultiKind) -> usize {
        self.ops.insert(OpState::Multi(Multi {
            kind,
            ring: RingId::Whole,
            queue: VecDeque::new(),
            waker: None,
            armed: false,
            pausing: false,
            orphan: false,
        }))
    }

    /// The ring a stream receive takes buffers from: the packed one where
    /// the kernel has it. Datagrams always take whole buffers.
    fn stream_ring(&self) -> RingId {
        if self.pool.is_registered(RingId::Packed) {
            RingId::Packed
        } else {
            RingId::Whole
        }
    }

    /// Makes sure the ring a receive of this kind takes buffers from holds
    /// one. `false` if no buffer is free for it.
    pub(crate) fn ensure_recv_buffer(&self, datagram: bool) -> bool {
        let ring = if datagram {
            RingId::Whole
        } else {
            self.stream_ring()
        };
        self.pool.ensure_published(Some(ring))
    }

    pub(crate) fn arm_multi(&mut self, key: usize, io: MultiIo) {
        let stream_ring = self.stream_ring();
        let (sqe, ring) = match io {
            MultiIo::Accept { fd } => (
                opcode::AcceptMulti::new(types::Fd(fd))
                    .flags(libc::SOCK_CLOEXEC)
                    .build(),
                RingId::Whole,
            ),
            MultiIo::Recv { fd } => (
                opcode::RecvMulti::new(types::Fd(fd), stream_ring.group()).build(),
                stream_ring,
            ),
            MultiIo::RecvMsg { fd, hdr } => (
                opcode::RecvMsgMulti::new(types::Fd(fd), hdr, RingId::Whole.group()).build(),
                RingId::Whole,
            ),
        };
        match self.ops.get_mut(key) {
            Some(OpState::Multi(m)) if !m.armed => {
                m.armed = true;
                m.ring = ring;
            }
            _ => panic!("giong: arming an invalid multishot operation"),
        }
        self.push(sqe.user_data(key as u64));
    }

    pub(crate) fn poll_multi(&mut self, key: usize, waker: &Waker) -> MultiPoll {
        let Some(OpState::Multi(m)) = self.ops.get_mut(key) else {
            panic!("giong: polling an invalid multishot operation");
        };
        match m.queue.pop_front() {
            Some(Queued::Raw(res)) => return MultiPoll::Item(res),
            Some(Queued::Piece { bid, off, len, .. }) => {
                self.pooled_queued -= 1;
                let pool = self.pool.clone();
                return MultiPoll::Data(RecvBuf::pooled(pool, bid, off, len));
            }
            Some(Queued::Heap(data)) => return MultiPoll::Data(RecvBuf::heap(data)),
            None => {}
        }
        if !m.armed {
            return MultiPoll::NeedArm;
        }
        match &mut m.waker {
            Some(w) if w.will_wake(waker) => {}
            slot => *slot = Some(waker.clone()),
        }
        MultiPoll::Pending
    }

    /// Called when the owner of a multishot operation (listener, stream) goes away.
    pub(crate) fn drop_multi(&mut self, key: usize) {
        let Some(OpState::Multi(m)) = self.ops.get_mut(key) else {
            return;
        };
        let kind = m.kind;
        let armed = m.armed;
        let queued = mem::take(&mut m.queue);
        m.waker = None;
        m.orphan = true;
        for item in queued {
            if let Queued::Piece { .. } = item {
                self.pooled_queued -= 1;
            }
            Self::release(&self.pool, kind, item);
        }
        if !armed {
            self.ops.remove(key);
        } else {
            // Still in flight: stop it; the entry is removed by its final CQE.
            // Always ask, even if a pause is already under way — a redundant
            // cancellation is harmless, a missing one would keep the socket
            // open for ever (the operation holds a reference to it).
            self.push(
                opcode::AsyncCancel::new(key as u64)
                    .build()
                    .user_data(UD_IGNORE),
            );
        }
    }

    /// Releases what an unconsumed multishot completion carries: an
    /// accepted descriptor, or a piece of a pool buffer.
    fn release(pool: &BufPool, kind: MultiKind, item: Queued) {
        match item {
            Queued::Raw(res) if kind == MultiKind::Accept && res >= 0 => {
                // SAFETY: an accepted descriptor nobody else knows about.
                unsafe { libc::close(res) };
            }
            Queued::Piece { bid, .. } => pool.release(bid),
            Queued::Raw(..) | Queued::Heap(_) => {}
        }
    }

    /// Moves received data that its reader left unread for a whole turn out
    /// of the buffer pool and onto the heap, so those buffers become
    /// available to the kernel again.
    ///
    /// Called when a receive finds the pool empty. Data that arrived in this
    /// turn stays: its reader has not had a chance to run yet, and on kernels
    /// without `DEFER_TASKRUN` one fast stream can fill the whole pool in a
    /// single turn; copying that out only lets the kernel fill the pool with
    /// the same stream again. The reader takes it in the coming turn, and if
    /// it does not, the next receive that finds the pool empty moves it.
    /// Costs a scan over all tracked operations, which is fine for something
    /// that only happens under buffer starvation.
    pub(crate) fn evacuate(&mut self) {
        if self.pooled_queued == 0 {
            return;
        }
        let pool = &self.pool;
        let now = self.turn;
        let mut kept = 0;
        for op in self.ops.iter_mut() {
            let OpState::Multi(m) = op else { continue };
            if m.kind != MultiKind::Recv {
                continue;
            }
            for item in m.queue.iter_mut() {
                if let Queued::Piece {
                    bid,
                    off,
                    len,
                    turn,
                } = *item
                {
                    if turn == now {
                        kept += 1;
                        continue;
                    }
                    *item = Queued::Heap(pool.copy_out(bid, off, len));
                    pool.release(bid);
                    self.recv_copied += u64::from(len);
                }
            }
        }
        self.pooled_queued = kept;
    }

    /// Closes a descriptor through the ring (batched with everything else).
    #[cfg(not(giong_tsan))]
    pub(crate) fn close_fd(&mut self, fd: RawFd) {
        self.push(
            opcode::Close::new(types::Fd(fd))
                .build()
                .user_data(UD_CLOSE),
        );
        self.pending_closes += 1;
    }

    /// ThreadSanitizer builds (`--cfg giong_tsan`, see `scripts/check.sh`) close
    /// descriptors with `close(2)` instead.
    ///
    /// TSan keeps its own table of descriptors, fed only by the libc calls it
    /// intercepts. A close through the ring is invisible to it, so when the
    /// kernel hands the same number to another thread, TSan still believes it
    /// belongs to the old socket and reports a race that does not exist.
    #[cfg(giong_tsan)]
    pub(crate) fn close_fd(&mut self, fd: RawFd) {
        self.submit_queued();
        // SAFETY: the caller owns `fd` and gives it up here, exactly as with
        // the ring-based close.
        unsafe { libc::close(fd) };
    }

    /// Hands every queued SQE to the kernel now. TSan builds call it before
    /// each close(2).
    ///
    /// Queued SQEs name descriptors by number, and the kernel looks the
    /// number up only when it takes the SQE. Once close(2) returns, another
    /// thread can get the same number for a new socket, and a receive still
    /// in the queue would take that socket's data. (A close through the ring
    /// comes after those SQEs in the same queue, so it needs no such step.)
    #[cfg(giong_tsan)]
    pub(crate) fn submit_queued(&mut self) {
        let mut attempts = 0u32;
        while !self.ring.submission().is_empty() {
            match self.enter(0, None) {
                Ok(()) => {}
                Err(e) => match e.raw_os_error() {
                    Some(libc::EINTR) => {}
                    // Completion queue backed up: make room, then retry.
                    Some(libc::EBUSY) | Some(libc::EAGAIN) => self.reap(),
                    _ => panic!("giong: io_uring submit failed: {e}"),
                },
            }
            attempts += 1;
            assert!(
                attempts < 100_000,
                "giong: unable to make progress submitting to io_uring"
            );
        }
    }

    pub(crate) fn pool(&self) -> &Rc<BufPool> {
        &self.pool
    }

    // ------------------------------------------------------------------ park

    /// `true` if completions are waiting to be collected: either already in
    /// the completion queue, or as deferred kernel work that the next
    /// `io_uring_enter` will turn into completions. Makes no system call.
    pub(crate) fn has_completions(&mut self) -> bool {
        if self.ring.submission().taskrun() {
            return true;
        }
        !self.ring.completion().is_empty()
    }

    fn arm_wake(&mut self) {
        let buf = &mut *self.wake_buf as *mut u64 as *mut u8;
        let sqe = opcode::Read::new(types::Fd(self.wake_fd), buf, 8)
            .build()
            .user_data(UD_WAKE);
        self.wake_armed = true;
        self.push(sqe);
    }

    /// Enters the kernel: submits queued SQEs, optionally waits, then dispatches
    /// completions. Returns resources that must be dropped outside the driver.
    pub(crate) fn park(&mut self, mode: Park) -> Vec<Box<dyn Any>> {
        self.turn = self.turn.wrapping_add(1);
        if !self.wake_armed && !self.closing {
            self.arm_wake();
        }
        let res = match mode {
            Park::Poll => {
                let (queued, pending) = {
                    let sq = self.ring.submission();
                    (sq.len(), sq.taskrun() || sq.cq_overflow())
                };
                if queued > 0 || pending {
                    self.enter(0, None)
                } else {
                    Ok(())
                }
            }
            Park::Wait(timeout) => self.enter(1, timeout),
        };
        if let Err(e) = res {
            match e.raw_os_error() {
                // Interrupted, timed out, or completion queue backed up (drained below).
                Some(libc::EINTR) | Some(libc::ETIME) | Some(libc::EBUSY) | Some(libc::EAGAIN) => {}
                _ => panic!("giong: io_uring_enter failed: {e}"),
            }
        }
        self.reap();
        mem::take(&mut self.graveyard)
    }

    /// `io_uring_enter`: submits every queued SQE, then waits until `want`
    /// completions are ready (0: does not wait), for at most `timeout`.
    ///
    /// The same as the `io-uring` crate's `submit_and_wait` and
    /// `submit_with_args`, except that it enters through the registered ring
    /// where there is one: the crate remembers a registration only in the
    /// short-lived `Submitter` that made it.
    fn enter(&mut self, want: u32, timeout: Option<Duration>) -> io::Result<()> {
        let (to_submit, pending) = {
            let mut sq = self.ring.submission();
            sq.sync();
            (sq.len() as u32, sq.taskrun() || sq.cq_overflow())
        };
        let (fd, mut flags) = match self.registered_ring {
            Some(index) => (index as i32, ENTER_REGISTERED_RING),
            None => (self.ring.as_raw_fd(), 0),
        };
        // Deferred task work and overflowed completions are only processed
        // by an enter that asks for completions.
        if want > 0 || pending {
            flags |= ENTER_GETEVENTS;
        }
        let ts;
        let arg;
        let (arg_ptr, arg_len) = match timeout {
            None => (std::ptr::null::<GeteventsArg>(), 0),
            Some(d) => {
                ts = KernelTimespec {
                    tv_sec: d.as_secs().min(i64::MAX as u64) as i64,
                    tv_nsec: i64::from(d.subsec_nanos()),
                };
                arg = GeteventsArg {
                    sigmask: 0,
                    sigmask_sz: 0,
                    min_wait_usec: 0,
                    ts: &ts as *const KernelTimespec as u64,
                };
                flags |= ENTER_EXT_ARG;
                (&arg as *const GeteventsArg, mem::size_of::<GeteventsArg>())
            }
        };
        // SAFETY: the SQEs up to the published tail are complete (`push`),
        // and `arg_ptr` is null or points at a `GeteventsArg` (and its
        // timespec) that outlives the call.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_io_uring_enter,
                fd,
                to_submit,
                want,
                flags,
                arg_ptr,
                arg_len,
            )
        };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Dispatches every completion currently in the completion queue.
    fn reap(&mut self) {
        let mut batch = mem::take(&mut self.cqes);
        loop {
            batch.clear();
            {
                let mut cq = self.ring.completion();
                cq.sync();
                for cqe in &mut cq {
                    batch.push((cqe.user_data(), cqe.result(), cqe.flags()));
                }
            }
            if batch.is_empty() {
                break;
            }
            for &(user_data, res, flags) in batch.iter() {
                self.dispatch(user_data, res, flags);
            }
            let to_pause = mem::take(&mut self.to_pause);
            for key in to_pause {
                self.push(
                    opcode::AsyncCancel::new(key as u64)
                        .build()
                        .user_data(UD_IGNORE),
                );
            }
            // Free buffers that are not in the ring yet go in before the
            // kernel can take more; only when there are none is the data of
            // streams nobody is reading moved out of the pool.
            if mem::take(&mut self.starved) && self.pool.no_spare() {
                self.evacuate();
            }
            self.pool.refill();
        }
        self.cqes = batch;
    }

    fn dispatch(&mut self, user_data: u64, res: i32, flags: u32) {
        let more = cqueue::more(flags);
        if !more {
            self.inflight = self.inflight.saturating_sub(1);
        }
        match user_data {
            UD_IGNORE => {}
            UD_WAKE => self.wake_armed = false,
            UD_CLOSE => self.pending_closes -= 1,
            key => {
                let key = key as usize;
                let remove = match self.ops.get_mut(key) {
                    None => false,
                    Some(OpState::Single(lc)) => {
                        Self::complete(lc, res, flags, more, &mut self.graveyard)
                    }
                    Some(OpState::Multi(m)) => {
                        if !more {
                            m.armed = false;
                        }
                        // What a receive brought: account for its buffer
                        // first, in the order the kernel posted completions.
                        let item = match cqueue::buffer_select(flags) {
                            Some(bid) if m.kind == MultiKind::Recv => {
                                let more_in_buf = flags & F_BUF_MORE != 0;
                                match self.pool.received(m.ring, bid, res, more_in_buf) {
                                    Received::Data { bid, off, len } => Queued::Piece {
                                        bid,
                                        off,
                                        len,
                                        turn: self.turn,
                                    },
                                    Received::Nothing => Queued::Raw(res),
                                }
                            }
                            _ => Queued::Raw(res),
                        };
                        if m.orphan {
                            Self::release(&self.pool, m.kind, item);
                            !more
                        } else {
                            let paused = !more && mem::replace(&mut m.pausing, false);
                            let recv = m.kind == MultiKind::Recv;
                            if recv && res == -libc::ENOBUFS {
                                // The ring ran dry. Not an error for the reader (its
                                // next poll re-arms); keep more buffers in the ring,
                                // and free up buffers after this batch.
                                self.pool.ran_dry(m.ring);
                                self.starved = true;
                                Self::release(&self.pool, m.kind, item);
                            } else if paused && res == -libc::ECANCELED {
                                // The cancellation we asked for when pausing.
                                Self::release(&self.pool, m.kind, item);
                            } else {
                                let limit = match m.kind {
                                    MultiKind::Accept => ACCEPT_BACKLOG_LIMIT,
                                    MultiKind::Recv => self.recv_limit,
                                };
                                // Behind: the oldest unread piece was already
                                // there in an earlier turn, so the reader ran
                                // and did not take it.
                                let behind = matches!(
                                    m.queue.front(),
                                    Some(Queued::Piece { turn, .. }) if *turn != self.turn
                                );
                                let pool_low = self.pool.free() * 4 < self.pool.count() as usize;
                                match item {
                                    Queued::Piece { bid, off, len, .. }
                                        if m.queue.len() >= limit && behind && pool_low =>
                                    {
                                        // Nobody is keeping up with this stream, and
                                        // others need buffers: do not let it sit on
                                        // pool buffers.
                                        m.queue.push_back(Queued::Heap(
                                            self.pool.copy_out(bid, off, len),
                                        ));
                                        self.pool.release(bid);
                                        self.recv_copied += u64::from(len);
                                    }
                                    item => {
                                        if let Queued::Piece { .. } = item {
                                            self.pooled_queued += 1;
                                        }
                                        m.queue.push_back(item);
                                    }
                                }
                                // Pause when the backlog reaches the limit — and ask again
                                // each time it grows by another `limit`, in case an
                                // earlier cancellation did not take.
                                if m.armed && m.queue.len() % limit == 0 {
                                    m.pausing = true;
                                    self.to_pause.push(key);
                                }
                            }
                            if let Some(waker) = m.waker.take() {
                                waker.wake();
                            }
                            false
                        }
                    }
                };
                if remove {
                    self.ops.remove(key);
                }
            }
        }
    }

    /// Records a completion of single-shot operation `lc`. Returns `true` if
    /// its slab entry can go.
    ///
    /// A zero-copy send completes twice: first with its result and
    /// `IORING_CQE_F_MORE`, then with a notification once the kernel no
    /// longer reads the buffer. A future with a [`ZeroCopyHold`] gets the
    /// result at the first (see [`poll_op`](Self::poll_op)), any other at
    /// the second.
    fn complete(
        lc: &mut Lifecycle,
        res: i32,
        flags: u32,
        more: bool,
        graveyard: &mut Vec<Box<dyn Any>>,
    ) -> bool {
        if more {
            // The result of a zero-copy send; the notification follows. A
            // dropped send stays `Ignored`: its buffer waits for it too.
            // The task is woken now, so that a future with a hold collects
            // the result here instead of at the notification, which comes
            // only once the peer has acknowledged every byte (up to a
            // delayed acknowledgement, 40 ms, later).
            if let Lifecycle::Waiting(waker) = lc {
                let waker = waker.clone();
                waker.wake_by_ref();
                *lc = Lifecycle::Sent(waker, res, flags & !F_MORE);
            }
            return false;
        }
        let done = match *lc {
            Lifecycle::Sent(_, sent, sent_flags) if flags & F_NOTIF != 0 => {
                let copied = if res as u32 & NOTIF_COPIED != 0 {
                    ZC_COPIED
                } else {
                    0
                };
                Lifecycle::Done(sent, sent_flags | copied)
            }
            _ => Lifecycle::Done(res, flags),
        };
        match mem::replace(lc, done) {
            Lifecycle::Waiting(waker) | Lifecycle::Sent(waker, ..) => {
                waker.wake();
                false
            }
            Lifecycle::Ignored(data) => {
                if let Some(data) = data {
                    if flags & F_NOTIF != 0 && res as u32 & NOTIF_COPIED != 0 {
                        if let Some(hold) = data.downcast_ref::<ZeroCopyHold>() {
                            hold.copied.set(true);
                        }
                    }
                    graveyard.push(data);
                }
                true
            }
            Lifecycle::Done(..) => false,
        }
    }

    // -------------------------------------------------------------- teardown

    /// Drains batched closes, cancels remaining I/O, and waits for its buffers.
    ///
    /// Returns `true` once no operation is in flight any more. On `false` the
    /// caller must leak the driver and the buffer pool instead of freeing them.
    pub(crate) fn shutdown(&mut self) -> bool {
        self.closing = true;
        for _ in 0..100 {
            // Push out queued SQEs (cancels, closes) and collect completions.
            match self.enter(0, None) {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::EINTR | libc::EBUSY | libc::EAGAIN)
                    ) => {}
                Err(_) => return false,
            }
            self.reap();
            if self.inflight == 0 {
                return true;
            }
            // A cancelled close can leave its descriptor open. Retrying a
            // failed close could also close a reused descriptor, so let the
            // batched closes finish before cancelling the remaining requests.
            if self.pending_closes == 0 {
                let _ = self.ring.submitter().register_sync_cancel(
                    Some(types::Timespec::from(Duration::from_millis(10))),
                    types::CancelBuilder::any().all(),
                );
            }
            let _ = self.enter(1, Some(Duration::from_millis(10)));
        }
        self.reap();
        self.inflight == 0
    }

    pub(crate) fn take_graveyard(&mut self) -> Vec<Box<dyn Any>> {
        mem::take(&mut self.graveyard)
    }

    /// SQEs whose final completion is still outstanding.
    pub(crate) fn inflight(&self) -> usize {
        self.inflight
    }

    /// Number of tracked operations (used by tests to detect leaks).
    pub(crate) fn tracked_ops(&self) -> usize {
        self.ops.len()
    }

    /// Received bytes moved out of the pool to the heap so far.
    pub(crate) fn recv_copied(&self) -> u64 {
        self.recv_copied
    }

    /// Kernel bugs worked around while setting the ring up.
    pub(crate) fn workarounds(&self) -> &[KernelWorkaround] {
        &self.workarounds
    }

    /// io_uring features this ring uses.
    pub(crate) fn features(&self) -> &[IoUringFeature] {
        &self.features
    }

    /// The smallest send that goes out without a copy, or `None` without
    /// [`IoUringFeature::ZeroCopySend`].
    pub(crate) fn zero_copy_min(&self) -> Option<usize> {
        self.zero_copy_min
    }
}

impl Drop for Uring {
    fn drop(&mut self) {
        // The thread's table holds a reference to the ring, which would keep
        // it (and its memory) alive until the thread exits.
        if let Some(index) = self.registered_ring {
            unregister_ring_fd(&self.ring, index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blocked or missing io_uring is explained; other errors pass through.
    #[test]
    fn setup_errors_say_why_io_uring_is_missing() {
        for errno in [libc::EPERM, libc::ENOSYS] {
            let original = io::Error::from_raw_os_error(errno);
            let kind = original.kind();
            let e = explain_setup_error(original);
            assert_eq!(e.kind(), kind);
            assert_eq!(crate::error::code(&e), Some(Code::IoUringUnavailable));
            let text = e.to_string();
            assert!(text.starts_with("GIONG-E0001: "), "{text}");
            assert!(text.contains("docs/DEPLOYMENT.md"), "{text}");
            let source = std::error::Error::source(e.get_ref().unwrap()).unwrap();
            let source = source.downcast_ref::<io::Error>().unwrap();
            assert_eq!(source.raw_os_error(), Some(errno));
        }
        let e = explain_setup_error(io::Error::from_raw_os_error(libc::ENOMEM));
        assert_eq!(e.raw_os_error(), Some(libc::ENOMEM));
    }

    /// An `ENOMEM` under a locked-memory limit names the limit and how to
    /// raise it; other errors are not touched.
    #[test]
    fn a_used_up_locked_memory_limit_is_named() {
        let enomem = io::Error::from_raw_os_error(libc::ENOMEM);
        let e = locked_memory_error(enomem, "rings", 8 << 20);
        assert_eq!(crate::error::code(&e), Some(Code::LockedMemoryLimit));
        assert_eq!(e.kind(), io::ErrorKind::OutOfMemory);
        let text = e.to_string();
        assert!(text.starts_with("GIONG-E0010: "), "{text}");
        assert!(text.contains("memory for its rings"), "{text}");
        assert!(text.contains("(RLIMIT_MEMLOCK, 8 MiB)"), "{text}");
        assert!(text.contains("Cannot allocate memory"), "{text}");
        for fix in [
            "LimitMEMLOCK=infinity",
            "--ulimit memlock=-1",
            "CAP_IPC_LOCK",
        ] {
            assert!(text.contains(fix), "{text}");
        }
        let source = std::error::Error::source(e.get_ref().unwrap()).unwrap();
        let source = source.downcast_ref::<io::Error>().unwrap();
        assert_eq!(source.raw_os_error(), Some(libc::ENOMEM));

        let e = explain_out_of_memory(io::Error::from_raw_os_error(libc::EINVAL), "rings");
        assert_eq!(e.raw_os_error(), Some(libc::EINVAL));
    }

    #[test]
    fn limits_are_written_in_the_largest_whole_unit() {
        assert_eq!(bytes(0), "0 bytes");
        assert_eq!(bytes(1000), "1000 bytes");
        assert_eq!(bytes(64 << 10), "64 KiB");
        assert_eq!(bytes(8 << 20), "8 MiB");
        assert_eq!(bytes((8 << 20) + 4096), "8196 KiB");
        assert_eq!(bytes(2 << 30), "2 GiB");
    }

    #[test]
    fn kernel_versions_are_read_from_the_release() {
        assert_eq!(kernel_version("6.8.0-147-generic"), Some((6, 8)));
        assert_eq!(kernel_version("5.15.225-0515225-generic"), Some((5, 15)));
        assert_eq!(kernel_version("7.0.0-1017-azure"), Some((7, 0)));
        assert_eq!(kernel_version("6.18.44-fc-v445"), Some((6, 18)));
        assert_eq!(kernel_version("6"), None);
        assert_eq!(kernel_version("unknown"), None);
    }

    /// A refused buffer ring is "too old" only on a kernel older than 6.0;
    /// on a newer one the error names the kernel instead of blaming its age.
    #[test]
    fn a_refused_buffer_ring_names_the_kernel() {
        let einval = || io::Error::from_raw_os_error(libc::EINVAL);
        let e = buffer_ring_error("5.15.0-100-generic", einval());
        assert_eq!(crate::error::code(&e), Some(Code::KernelTooOld));
        assert!(
            e.to_string().contains("this kernel is 5.15.0-100-generic"),
            "{e}"
        );

        for release in ["6.8.0-147-generic", "unknown"] {
            let e = buffer_ring_error(release, einval());
            assert_eq!(crate::error::code(&e), Some(Code::BufferRingRefused));
            let text = e.to_string();
            assert!(text.starts_with("GIONG-E0009: "), "{text}");
            assert!(text.contains(&format!("this kernel ({release})")), "{text}");
            assert!(!text.contains("too old"), "{text}");
            assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
            let source = std::error::Error::source(e.get_ref().unwrap()).unwrap();
            let source = source.downcast_ref::<io::Error>().unwrap();
            assert_eq!(source.raw_os_error(), Some(libc::EINVAL));
        }
    }

    /// The buffer ring registers on any kernel from 6.0 on, and the
    /// workaround for the inverted reserved-field check is used exactly when
    /// the probe finds that check: never on a correct kernel.
    #[test]
    fn the_buffer_ring_registers_with_a_workaround_only_where_needed() {
        let Ok(ring) = build_ring_from(64, RingTier::Basic) else {
            eprintln!("skipped: io_uring is not available here");
            return;
        };
        let pool = BufPool::new(64, 1024).unwrap();
        // SAFETY: `pool` outlives `ring` (dropped first, below).
        let res = unsafe { register_buf_ring(&ring, &pool) };
        let release = kernel_release();
        match res {
            Ok(w) => {
                let inverted = registers_with_reserved_bits(ring.as_raw_fd());
                assert_eq!(
                    w.contains(&KernelWorkaround::BufferRingReservedCheck),
                    inverted,
                    "{release}: {w:?}"
                );
                // scripts/kernels.sh boots kernels whose bugs are known.
                if let Ok(names) = std::env::var("GIONG_TEST_EXPECT_KERNEL_WORKAROUNDS") {
                    let got: Vec<&str> = w.iter().map(|w| w.name()).collect();
                    assert_eq!(got.join(","), names, "{release}");
                }
            }
            // Older than 6.0: no buffer rings to work around.
            Err(e) => {
                assert_eq!(crate::error::code(&e), Some(Code::KernelTooOld), "{e}");
                assert!(kernel_version(&release).is_some_and(|v| v < (6, 0)), "{e}");
            }
        }
        drop(ring);
    }

    /// The result of a zero-copy send wakes the task that waits for it, so
    /// that a write leaving a `ZeroCopyHold` returns then. Without the wake
    /// it returned only at the notification, after the peer acknowledged
    /// the data: one connection sending 16 MiB from memory that never
    /// changes stalled for 40 ms (a delayed acknowledgement) on about one
    /// response in five on bare metal. Loopback copies and notifies at once,
    /// so no test over a real socket here can tell the difference.
    #[test]
    fn the_result_of_a_zero_copy_send_wakes_its_task() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let woken = Arc::new(Count(AtomicUsize::new(0)));
        let mut lc = Lifecycle::Waiting(Waker::from(woken.clone()));
        let mut graveyard = Vec::new();

        assert!(!Uring::complete(
            &mut lc,
            65536,
            F_MORE,
            true,
            &mut graveyard
        ));
        assert_eq!(
            woken.0.load(Ordering::SeqCst),
            1,
            "the result must wake the task"
        );
        assert!(matches!(lc, Lifecycle::Sent(_, 65536, 0)));

        // A task that did not collect it is woken again by the notification.
        assert!(!Uring::complete(&mut lc, 0, F_NOTIF, false, &mut graveyard));
        assert_eq!(woken.0.load(Ordering::SeqCst), 2);
        assert!(matches!(lc, Lifecycle::Done(65536, 0)));

        // A send whose future already returned (`Ignored`) wakes nobody,
        // and its hold goes when the notification comes.
        let mut lc = Lifecycle::Ignored(Some(Box::new(())));
        assert!(!Uring::complete(
            &mut lc,
            65536,
            F_MORE,
            true,
            &mut graveyard
        ));
        assert!(Uring::complete(&mut lc, 0, F_NOTIF, false, &mut graveyard));
        assert_eq!(graveyard.len(), 1);
        assert_eq!(woken.0.load(Ordering::SeqCst), 2);
    }

    /// Every setup tier must produce a working ring on a current kernel, so
    /// that `GIONG_RING_TIER` really exercises the older-kernel paths.
    #[test]
    fn every_ring_tier_builds() {
        // Where io_uring is blocked (a container's seccomp profile, a
        // sandbox) there is no ring to build; giong runs on epoll there, and
        // tests/blocked.rs covers that.
        if let Err(e) = build_ring_from(64, RingTier::Basic) {
            if matches!(
                e.raw_os_error(),
                Some(libc::EPERM | libc::EACCES | libc::ENOSYS)
            ) {
                eprintln!("skipped: io_uring is blocked here ({e})");
                return;
            }
        }
        // `SINGLE_ISSUER` is only requested by the preferred tier.
        let coop = build_ring_from(64, RingTier::Coop).expect("coop tier");
        assert!(!coop.params().is_setup_single_issuer());

        let basic = build_ring_from(64, RingTier::Basic).expect("basic tier");
        assert!(!basic.params().is_setup_single_issuer());

        // The preferred tier may legitimately fall back on an old kernel, but
        // it must always yield a ring.
        build_ring_from(64, RingTier::Defer).expect("default tier");
    }
}
