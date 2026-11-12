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
use std::os::fd::RawFd;
use std::rc::Rc;
use std::task::{Poll, Waker};
use std::time::Duration;

use io_uring::{cqueue, opcode, squeue, types, IoUring};

use crate::buf::BufPool;
use crate::error::{Code, Error};
use crate::slab::Slab;

/// Buffer group id of the worker's receive pool.
pub(crate) const BGID: u16 = 0;

/// `user_data` of operations whose completion nobody waits for, such as cancels.
const UD_IGNORE: u64 = u64::MAX;
/// `user_data` of the eventfd read used for cross-thread wake-ups.
const UD_WAKE: u64 = u64::MAX - 1;
/// `user_data` of batched closes, which must finish before shutdown cancels I/O.
const UD_CLOSE: u64 = u64::MAX - 2;

/// Backpressure for multishot receive. Once this many completions are queued
/// for a stream that is not being read fast enough:
///
/// * the multishot receive is paused (cancelled; re-armed when the queue has
///   been drained), which pushes back on the peer through TCP flow control, and
/// * anything that still arrives is copied to the heap so that its pool buffer
///   can be recycled immediately.
///
/// Together with [`Driver::evacuate`] this guarantees that slow or stalled
/// connections cannot pin the shared buffer pool.
const RECV_BACKLOG_LIMIT: usize = 8;

/// A multishot accept is paused once this many accepted connections are
/// waiting for the application; further connections then wait in the
/// kernel's listen backlog instead of piling up as open descriptors.
const ACCEPT_BACKLOG_LIMIT: usize = 1024;

fn backlog_limit(kind: MultiKind) -> usize {
    match kind {
        MultiKind::Accept => ACCEPT_BACKLOG_LIMIT,
        MultiKind::Recv => RECV_BACKLOG_LIMIT,
    }
}

/// `true` if a queued receive completion holds a pool buffer with data.
fn holds_pool_buffer(kind: MultiKind, res: i32, flags: u32) -> bool {
    kind == MultiKind::Recv && res > 0 && cqueue::buffer_select(flags).is_some()
}

/// How the event loop wants to enter the kernel.
pub(crate) enum Park {
    /// Submit and collect completions without blocking.
    Poll,
    /// Block until a completion arrives, or for at most the given duration.
    Wait(Option<Duration>),
}

enum Lifecycle {
    /// In flight; the waker of the task awaiting it.
    Waiting(Waker),
    /// Finished; the result has not been collected yet.
    Done(i32, u32),
    /// The future was dropped while in flight. Holds the operation's resources
    /// until the kernel is done with them.
    Ignored(Option<Box<dyn Any>>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MultiKind {
    Accept,
    Recv,
}

/// A multishot completion waiting to be consumed.
enum Queued {
    /// `(result, flags)` exactly as the kernel reported it. For a receive
    /// with data, the flags name the pool buffer that holds it.
    Raw(i32, u32),
    /// Received data that was moved out of the pool.
    Heap(Vec<u8>),
}

struct Multi {
    kind: MultiKind,
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

/// Result of polling a multishot operation.
pub(crate) enum MultiPoll {
    Item(i32, u32),
    /// Received data that lives on the heap rather than in a pool buffer.
    Heap(Vec<u8>),
    /// Nothing queued and no SQE in flight: the caller must arm it.
    NeedArm,
    Pending,
}

pub(crate) struct Driver {
    ring: IoUring,
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
    /// Queued receive completions that still hold a pool buffer.
    pooled_queued: usize,
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
    build_ring_from(entries, first_tier()).map_err(explain_setup_error)
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
                "allow io_uring for this process. Docker's default seccomp profile blocks it, \
                 sandboxes such as gVisor do not have it, and the kernel.io_uring_disabled \
                 sysctl turns it off; docs/DEPLOYMENT.md shows how to allow it in Docker and \
                 Kubernetes.",
            )
            .with_source(e),
        ),
        _ => e,
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

impl Driver {
    pub(crate) fn new(entries: u32, pool: Rc<BufPool>, wake_fd: RawFd) -> io::Result<Self> {
        let ring = build_ring(entries)?;
        // SAFETY: the ring memory is page aligned, holds `count` entries and is
        // kept alive by `pool`, which outlives the ring (see `Worker` teardown).
        unsafe {
            ring.submitter()
                .register_buf_ring_with_flags(pool.ring_addr(), pool.count(), BGID, 0)
                .map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        Error::new(
                            Code::KernelTooOld,
                            format!("giong needs io_uring buffer rings, from Linux 6.0 ({e})"),
                            "run on Linux 6.0 or newer (6.1 or newer is faster).",
                        )
                        .with_source(e),
                    )
                })?;
        }
        Ok(Self {
            ring,
            ops: Slab::new(),
            inflight: 0,
            pending_closes: 0,
            pool,
            cqes: Vec::with_capacity(256),
            to_pause: Vec::new(),
            starved: false,
            pooled_queued: 0,
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
            match self.ring.submit() {
                Ok(_) => {}
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
    pub(crate) fn submit_op(&mut self, sqe: squeue::Entry, waker: &Waker) -> usize {
        let key = self
            .ops
            .insert(OpState::Single(Lifecycle::Waiting(waker.clone())));
        self.push(sqe.user_data(key as u64));
        key
    }

    /// Polls a single-shot operation, consuming its entry when it is done.
    pub(crate) fn poll_op(&mut self, key: usize, waker: &Waker) -> Poll<(i32, u32)> {
        match self.ops.get_mut(key) {
            Some(OpState::Single(Lifecycle::Done(res, flags))) => {
                let out = (*res, *flags);
                self.ops.remove(key);
                Poll::Ready(out)
            }
            Some(OpState::Single(Lifecycle::Waiting(w))) => {
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
            queue: VecDeque::new(),
            waker: None,
            armed: false,
            pausing: false,
            orphan: false,
        }))
    }

    pub(crate) fn arm_multi(&mut self, key: usize, sqe: squeue::Entry) {
        match self.ops.get_mut(key) {
            Some(OpState::Multi(m)) if !m.armed => m.armed = true,
            _ => panic!("giong: arming an invalid multishot operation"),
        }
        self.push(sqe.user_data(key as u64));
    }

    pub(crate) fn poll_multi(&mut self, key: usize, waker: &Waker) -> MultiPoll {
        let Some(OpState::Multi(m)) = self.ops.get_mut(key) else {
            panic!("giong: polling an invalid multishot operation");
        };
        match m.queue.pop_front() {
            Some(Queued::Raw(res, flags)) => {
                if holds_pool_buffer(m.kind, res, flags) {
                    self.pooled_queued -= 1;
                }
                return MultiPoll::Item(res, flags);
            }
            Some(Queued::Heap(data)) => return MultiPoll::Heap(data),
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
            if let Queued::Raw(res, flags) = item {
                if holds_pool_buffer(kind, res, flags) {
                    self.pooled_queued -= 1;
                }
                Self::release(&self.pool, kind, res, flags);
            }
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

    /// Releases what an unconsumed multishot completion carries.
    fn release(pool: &BufPool, kind: MultiKind, res: i32, flags: u32) {
        match kind {
            MultiKind::Accept => {
                if res >= 0 {
                    // SAFETY: an accepted descriptor nobody else knows about.
                    unsafe { libc::close(res) };
                }
            }
            MultiKind::Recv => {
                if let Some(bid) = cqueue::buffer_select(flags) {
                    pool.recycle(bid);
                }
            }
        }
    }

    /// Moves all received-but-unread data out of the buffer pool and onto the
    /// heap, so every buffer that is not in the hands of user code becomes
    /// available to the kernel again.
    ///
    /// Called when a receive finds the pool empty. Costs a scan over all
    /// tracked operations, which is fine for something that only happens
    /// under buffer starvation.
    pub(crate) fn evacuate(&mut self) {
        if self.pooled_queued == 0 {
            return;
        }
        self.pooled_queued = 0;
        let pool = &self.pool;
        for op in self.ops.iter_mut() {
            let OpState::Multi(m) = op else { continue };
            if m.kind != MultiKind::Recv {
                continue;
            }
            for item in m.queue.iter_mut() {
                if let Queued::Raw(res, flags) = *item {
                    if let (true, Some(bid)) = (res > 0, cqueue::buffer_select(flags)) {
                        *item = Queued::Heap(pool.copy_out(bid, res as usize));
                        pool.recycle(bid);
                    }
                }
            }
        }
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
        // SAFETY: the caller owns `fd` and gives it up here, exactly as with
        // the ring-based close.
        unsafe { libc::close(fd) };
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
                    self.ring.submit_and_wait(0).map(drop)
                } else {
                    Ok(())
                }
            }
            Park::Wait(None) => self.ring.submit_and_wait(1).map(drop),
            Park::Wait(Some(dur)) => {
                let ts = types::Timespec::from(dur);
                let args = types::SubmitArgs::new().timespec(&ts);
                self.ring.submitter().submit_with_args(1, &args).map(drop)
            }
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
            if mem::take(&mut self.starved) {
                self.evacuate();
            }
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
                        match mem::replace(lc, Lifecycle::Done(res, flags)) {
                            Lifecycle::Waiting(waker) => {
                                waker.wake();
                                false
                            }
                            Lifecycle::Ignored(data) => {
                                if let Some(data) = data {
                                    self.graveyard.push(data);
                                }
                                true
                            }
                            Lifecycle::Done(..) => false,
                        }
                    }
                    Some(OpState::Multi(m)) => {
                        if m.kind == MultiKind::Recv && cqueue::buffer_select(flags).is_some() {
                            self.pool.taken();
                        }
                        if !more {
                            m.armed = false;
                        }
                        if m.orphan {
                            Self::release(&self.pool, m.kind, res, flags);
                            !more
                        } else {
                            let paused = !more && mem::replace(&mut m.pausing, false);
                            let recv = m.kind == MultiKind::Recv;
                            if recv && res == -libc::ENOBUFS {
                                // The pool ran dry. Not an error for the reader (its
                                // next poll re-arms); free up buffers after this batch.
                                self.starved = true;
                                Self::release(&self.pool, m.kind, res, flags);
                            } else if paused && res == -libc::ECANCELED {
                                // The cancellation we asked for when pausing.
                                Self::release(&self.pool, m.kind, res, flags);
                            } else {
                                let limit = backlog_limit(m.kind);
                                match cqueue::buffer_select(flags) {
                                    Some(bid) if recv && res > 0 && m.queue.len() >= limit => {
                                        // Nobody is keeping up with this stream: do not
                                        // let it sit on pool buffers.
                                        m.queue.push_back(Queued::Heap(
                                            self.pool.copy_out(bid, res as usize),
                                        ));
                                        self.pool.recycle(bid);
                                    }
                                    _ => {
                                        if holds_pool_buffer(m.kind, res, flags) {
                                            self.pooled_queued += 1;
                                        }
                                        m.queue.push_back(Queued::Raw(res, flags));
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

    // -------------------------------------------------------------- teardown

    /// Drains batched closes, cancels remaining I/O, and waits for its buffers.
    ///
    /// Returns `true` once no operation is in flight any more. On `false` the
    /// caller must leak the driver and the buffer pool instead of freeing them.
    pub(crate) fn shutdown(&mut self) -> bool {
        self.closing = true;
        for _ in 0..100 {
            // Push out queued SQEs (cancels, closes) and collect completions.
            match self.ring.submit_and_wait(0) {
                Ok(_) => {}
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
            let ts = types::Timespec::from(Duration::from_millis(10));
            let args = types::SubmitArgs::new().timespec(&ts);
            let _ = self.ring.submitter().submit_with_args(1, &args);
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

    /// Every setup tier must produce a working ring on a current kernel, so
    /// that `GIONG_RING_TIER` really exercises the older-kernel paths.
    #[test]
    fn every_ring_tier_builds() {
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
