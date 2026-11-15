//! The readiness drivers: epoll on Linux, for machines where io_uring is
//! blocked or missing, and kqueue on macOS. Both are this one driver over a
//! different [`Poller`]: everything below is written once, and the poller
//! only registers descriptors and reports which became ready. The poller is
//! a type parameter, so each build has direct calls and no indirection.
//!
//! Operations are tried at once with a non-blocking system call. One that
//! would block waits for its descriptor to become ready and is tried again
//! the next time its future is polled. Each descriptor is registered once,
//! edge-triggered for reading and writing, the first time an operation uses
//! it, and set to non-blocking mode then.
//!
//! * Multishot accept and receive become "try on poll": nothing is read
//!   until the owner asks, so data waits in the kernel's socket buffer and
//!   TCP flow control does the pushing back. A receive fills the buffer the
//!   kernel would have taken from the worker's pool, and reports it with
//!   io_uring's completion flags, so the socket code has one path for both
//!   drivers.
//! * File operations run on the blocking threads ([`super::blocking`]): a
//!   regular file is always "ready", so waiting for the disk on the worker
//!   would stall every connection on it.
//! * Cancelling an operation that waits for readiness frees its resources at
//!   once: no system call is in progress. One on a blocking thread keeps them
//!   until the thread is done, exactly like an io_uring operation in flight.
//! * A descriptor that blocking threads are using is closed only once the
//!   last of them has reported. io_uring operations hold a reference to the
//!   file; a thread holds only the number, which the kernel would otherwise
//!   hand to the next `open` while the thread is about to use it.
//!
//! An operation that finishes at once does not yield, so a task whose every
//! read and write succeeds immediately could keep the worker to itself. A
//! budget of [`BUDGET`] immediate operations per task poll prevents that:
//! once it is spent, operations report "pending" and wake their task again,
//! which puts it at the back of the run queue.

use std::any::Any;
use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

use super::blocking::{self, Cancel, Completions};
use super::{Io, MultiIo, MultiKind, MultiPoll, Park, Start, BUFFER_SHIFT, F_BUFFER, F_MORE};
use crate::buf::{BufPool, RecvBuf};
use crate::slab::Slab;
use crate::sys;

/// What the readiness driver needs from the kernel's event queue (epoll,
/// kqueue). Tokens are 64 bits: [`WAKE`] and [`DONE`] for the two bells,
/// otherwise a descriptor number in the lower half and a generation in the
/// upper half.
pub(crate) trait Poller: Sized {
    /// Creates the event queue.
    fn new() -> io::Result<Self>;
    /// Watches a bell for readability, level-triggered.
    fn watch(&self, fd: RawFd, token: u64) -> io::Result<()>;
    /// Registers a descriptor for reading and writing, edge-triggered;
    /// `-errno` on failure. Registering a number again replaces the old
    /// registration.
    fn register(&self, fd: RawFd, token: u64) -> Result<(), i32>;
    /// Waits for events, for at most `timeout` (`None`: no limit), and
    /// returns how many arrived. An interrupted wait returns 0; an error the
    /// event loop cannot go on after stops the worker
    /// ([`super::event_loop_failed`]).
    fn wait(&mut self, timeout: Option<Duration>) -> usize;
    /// Event `i` of the last wait: its token, and whether it means
    /// "readable" and "writable" (end of stream and errors count as both
    /// where they apply).
    fn event(&self, i: usize) -> (u64, bool, bool);
}

/// Immediate operations a task may complete in one poll before it yields.
pub(crate) const BUDGET: u32 = 128;

thread_local! {
    /// What is left of the current task's [`BUDGET`].
    static LEFT: Cell<u32> = const { Cell::new(BUDGET) };
}

/// Gives the task about to be polled a full budget. Called by the executor.
pub(crate) fn refill_budget() {
    LEFT.with(|left| left.set(BUDGET));
}

/// Takes one unit of the current task's budget; `false` if it is spent.
fn spend_budget() -> bool {
    LEFT.with(|left| match left.get() {
        0 => false,
        n => {
            left.set(n - 1);
            true
        }
    })
}

/// Token of the worker's wake-up bell (cross-thread wake-ups).
pub(crate) const WAKE: u64 = u64::MAX;
/// Token of the bell the blocking threads ring.
pub(crate) const DONE: u64 = u64::MAX - 1;

/// Events collected per wait.
pub(crate) const MAX_EVENTS: usize = 1024;

/// Which readiness an operation waits for.
#[derive(Clone, Copy)]
enum Dir {
    Read,
    Write,
}

/// What the driver knows about a descriptor number.
enum Slot {
    /// Registered with the poller.
    Pollable(Registration),
    /// A regular file, a directory or a device: its reads and writes go to
    /// the blocking threads.
    Blocking,
}

/// One registered descriptor and the tasks waiting for it.
struct Registration {
    /// Matches the upper half of the token, so that events for a
    /// descriptor number that has since been closed and reused are ignored.
    gen: u32,
    readers: Vec<Waker>,
    writers: Vec<Waker>,
}

enum Lifecycle {
    Waiting(Waker),
    Done(i32),
    /// The future was dropped; holds the resources until the thread is done.
    /// With `closes_result`, a result that is not negative is a descriptor
    /// nobody owns (an open), which the driver closes.
    Ignored {
        data: Option<Box<dyn Any>>,
        closes_result: bool,
    },
}

enum EOp {
    /// Waits for its descriptor; tried again when its future is polled.
    Ready { io: Io, connecting: bool },
    /// Runs on a blocking thread, on descriptor `fd` (-1 for none).
    Blocking {
        state: Lifecycle,
        cancel: Arc<Cancel>,
        fd: RawFd,
    },
    /// A multishot operation, armed once `io` is set.
    Multi { io: Option<MultiIo> },
}

/// What a non-blocking attempt came to.
enum Attempt {
    Done(i32),
    Wait(RawFd, Dir),
}

use sys::neg_errno;

/// Runs `f` until it is not interrupted by a signal; `-errno` on failure.
fn retry(mut f: impl FnMut() -> isize) -> i32 {
    loop {
        let r = f();
        if r >= 0 {
            return r.min(i32::MAX as isize) as i32;
        }
        let e = neg_errno();
        if e != -libc::EINTR {
            return e;
        }
    }
}

fn is_would_block(res: i32) -> bool {
    res == -libc::EAGAIN || res == -libc::EWOULDBLOCK
}

/// An operation for a blocking thread; its pointers are only used there
/// while the driver keeps what they point at alive.
struct SendIo(Io);

// SAFETY: the memory behind the pointers stays put and untouched by the
// worker until the job reports completion (see the module docs).
unsafe impl Send for SendIo {}

impl SendIo {
    fn run(self) -> i32 {
        // SAFETY: see `SendIo`; the pointers come from `Op` resources.
        unsafe { run_blocking(self.0) }
    }
}

/// Performs a file operation with a blocking system call.
///
/// # Safety
///
/// The pointers in `io` must be valid for the call.
unsafe fn run_blocking(io: Io) -> i32 {
    match io {
        Io::Read {
            fd,
            buf,
            len,
            offset,
        } => {
            let res = retry(|| sys::pread(fd, buf, len as usize, offset));
            if res == -libc::ESPIPE {
                // A pipe or a device: no offsets, as with io_uring.
                retry(|| libc::read(fd, buf.cast(), len as usize))
            } else {
                res
            }
        }
        Io::Write {
            fd,
            buf,
            len,
            offset,
        } => {
            let res = retry(|| sys::pwrite(fd, buf, len as usize, offset));
            if res == -libc::ESPIPE {
                retry(|| libc::write(fd, buf.cast(), len as usize))
            } else {
                res
            }
        }
        Io::OpenAt { path, flags, mode } => {
            retry(|| libc::openat(libc::AT_FDCWD, path, flags, mode) as isize)
        }
        // A close is never retried: after EINTR the descriptor is gone.
        Io::Close { fd } => match libc::close(fd) {
            0 => 0,
            _ => neg_errno(),
        },
        Io::Statx {
            dir,
            path,
            flags,
            mask,
            out,
        } => retry(|| sys::stat_at(dir, path, flags, mask, out)),
        Io::Fsync { fd, datasync } => retry(|| sys::fsync(fd, datasync)),
        Io::Fallocate { fd, offset, len } => retry(|| sys::fallocate(fd, offset, len)),
        Io::UnlinkAt { path, flags } => {
            retry(|| libc::unlinkat(libc::AT_FDCWD, path, flags) as isize)
        }
        Io::MkDirAt { path, mode } => {
            retry(|| libc::mkdirat(libc::AT_FDCWD, path, mode as libc::mode_t) as isize)
        }
        Io::RenameAt { from, to } => {
            retry(|| libc::renameat(libc::AT_FDCWD, from, libc::AT_FDCWD, to) as isize)
        }
        Io::Connect { .. }
        | Io::Recv { .. }
        | Io::Send { .. }
        | Io::Writable { .. }
        | Io::SendMsg { .. }
        | Io::RecvMsg { .. }
        | Io::ReadReady { .. } => unreachable!("socket operations never block a thread"),
    }
}

/// The readiness driver over poller `P`.
pub(crate) struct Readiness<P: Poller> {
    poller: P,
    pool: Rc<BufPool>,
    /// The read end of the worker's wake-up bell, rung by threads that wake
    /// it.
    wake_fd: RawFd,
    completions: Arc<Completions>,
    ops: Slab<EOp>,
    /// What is known about each descriptor, indexed by its number.
    fds: Vec<Option<Slot>>,
    next_gen: u32,
    /// Events collected by `has_completions` and not dispatched yet.
    ready: usize,
    done: Vec<(usize, i32)>,
    /// Blocking jobs whose result has not been collected yet.
    blocking: usize,
    /// Descriptors that blocking jobs use: how many jobs, and whether the
    /// descriptor is to be closed when the last one reports.
    busy: HashMap<RawFd, (u32, bool)>,
    graveyard: Vec<Box<dyn Any>>,
}

impl<P: Poller> Readiness<P> {
    pub(crate) fn new(pool: Rc<BufPool>, wake_fd: RawFd) -> io::Result<Self> {
        let poller = P::new()?;
        let completions = Completions::new()?;
        let driver = Self {
            poller,
            pool,
            wake_fd,
            completions,
            ops: Slab::new(),
            fds: Vec::new(),
            next_gen: 1,
            ready: 0,
            done: Vec::new(),
            blocking: 0,
            busy: HashMap::new(),
            graveyard: Vec::new(),
        };
        // The worker's bell is drained after every wake-up, so it must not
        // block when another thread got there first. (Under Miri, which has
        // no `fcntl` on eventfds, it is created non-blocking instead.)
        #[cfg(not(miri))]
        set_nonblocking(wake_fd)?;
        driver.poller.watch(wake_fd, WAKE)?;
        driver.poller.watch(driver.completions.fd(), DONE)?;
        Ok(driver)
    }

    pub(crate) fn pool(&self) -> &Rc<BufPool> {
        &self.pool
    }

    // ------------------------------------------------------------ descriptors

    /// Registers `fd` with the poller (once) and makes it non-blocking.
    fn register(&mut self, fd: RawFd) -> Result<(), i32> {
        let Ok(i) = usize::try_from(fd) else {
            return Err(-libc::EBADF);
        };
        if matches!(self.fds.get(i), Some(Some(Slot::Pollable(_)))) {
            return Ok(());
        }
        set_nonblocking(fd).map_err(|e| -e.raw_os_error().unwrap_or(libc::EIO))?;
        let gen = self.next_gen;
        self.next_gen = self.next_gen.wrapping_add(1);
        self.poller
            .register(fd, (u64::from(gen) << 32) | u64::from(fd as u32))?;
        self.set_slot(
            i,
            Slot::Pollable(Registration {
                gen,
                readers: Vec::new(),
                writers: Vec::new(),
            }),
        );
        Ok(())
    }

    fn set_slot(&mut self, i: usize, slot: Slot) {
        if self.fds.len() <= i {
            self.fds.resize_with(i + 1, || None);
        }
        self.fds[i] = Some(slot);
    }

    /// `true` if reads and writes on file descriptor `fd` can wait for
    /// readiness: a pipe, a FIFO or a socket. Those are made non-blocking
    /// and registered. Regular files, directories and devices (a terminal
    /// may be shared with the shell, whose mode must not change) go to the
    /// blocking threads, and so do pipes on standard input, output and
    /// error: a shell pipeline shares them with the processes on either
    /// side, and non-blocking mode is a property of the pipe, not of this
    /// process (`cat` on the other end would fail with `EAGAIN`). Decided
    /// once per descriptor.
    fn pollable_file(&mut self, fd: RawFd) -> bool {
        let Ok(i) = usize::try_from(fd) else {
            return false;
        };
        match self.fds.get(i) {
            Some(Some(Slot::Pollable(_))) => return true,
            Some(Some(Slot::Blocking)) => return false,
            _ => {}
        }
        let streams = match sys::fd_mode(fd).map(|m| m & sys::mode::S_IFMT) {
            Some(sys::mode::S_IFSOCK) => true,
            Some(sys::mode::S_IFIFO) => fd > 2,
            _ => false,
        };
        if streams && self.register(fd).is_ok() {
            return true;
        }
        self.set_slot(i, Slot::Blocking);
        false
    }

    /// Forgets what is known about `fd`, which is being closed.
    fn forget(&mut self, fd: RawFd) {
        if let Some(slot) = usize::try_from(fd).ok().and_then(|i| self.fds.get_mut(i)) {
            *slot = None;
        }
    }

    /// Wakes `waker` when `fd` becomes ready in direction `dir`.
    fn wait(&mut self, fd: RawFd, dir: Dir, waker: &Waker) {
        let Some(Some(Slot::Pollable(reg))) = self.fds.get_mut(fd as usize) else {
            // Not registered (cannot happen after a successful attempt):
            // try again on the next turn rather than hang.
            waker.wake_by_ref();
            return;
        };
        let list = match dir {
            Dir::Read => &mut reg.readers,
            Dir::Write => &mut reg.writers,
        };
        if !list.iter().any(|w| w.will_wake(waker)) {
            list.push(waker.clone());
        }
    }

    /// Closes a descriptor the caller gives up: now, or when the last
    /// blocking job using it reports.
    pub(crate) fn close_fd(&mut self, fd: RawFd) {
        self.forget(fd);
        if let Some((_, close)) = self.busy.get_mut(&fd) {
            *close = true;
            return;
        }
        // SAFETY: the caller owns `fd` and gives it up here. Closing also
        // removes it from the poller (no other descriptor shares it).
        unsafe { libc::close(fd) };
    }

    /// A blocking job on `fd` has reported; closes `fd` if it was given up
    /// and no other job uses it.
    fn release(&mut self, fd: RawFd) {
        let Some((jobs, close)) = self.busy.get_mut(&fd) else {
            return;
        };
        *jobs -= 1;
        if *jobs > 0 {
            return;
        }
        let close = *close;
        self.busy.remove(&fd);
        if close {
            // SAFETY: the owner gave `fd` up in `close_fd`, and the last job
            // that used it is done.
            unsafe { libc::close(fd) };
        }
    }

    // ---------------------------------------------------------------- attempts

    /// Tries a socket operation without blocking.
    fn attempt(&mut self, io: &Io, connecting: &mut bool) -> Attempt {
        let fd = match *io {
            Io::Connect { fd, .. }
            | Io::Recv { fd, .. }
            | Io::Send { fd, .. }
            | Io::Writable { fd }
            | Io::SendMsg { fd, .. }
            | Io::RecvMsg { fd, .. }
            | Io::ReadReady { fd, .. }
            | Io::Read { fd, .. }
            | Io::Write { fd, .. } => fd,
            _ => unreachable!("file operations go to the blocking threads"),
        };
        if let Err(e) = self.register(fd) {
            return Attempt::Done(e);
        }
        // SAFETY: for every call below, the pointers come from `Op`
        // resources, valid while the operation exists; the descriptor is
        // non-blocking.
        let (res, dir) = unsafe {
            match *io {
                Io::Connect { fd, addr, len } => {
                    if *connecting {
                        return Attempt::Done(connect_result(fd)).or_wait(fd, Dir::Write);
                    }
                    let res = match libc::connect(fd, addr, len) {
                        0 => 0,
                        _ => neg_errno(),
                    };
                    if res == -libc::EINPROGRESS || res == -libc::EINTR {
                        // In progress: done when the socket becomes writable.
                        *connecting = true;
                        return Attempt::Wait(fd, Dir::Write);
                    }
                    // EAGAIN (a Unix socket whose listener's backlog is full)
                    // means "try again": the connect is repeated when ready.
                    (res, Dir::Write)
                }
                Io::Writable { fd } => {
                    // The caller just got EAGAIN, so the socket is full: wait
                    // for the next "writable" edge, then let the caller try
                    // again (`connecting` marks a wait that has begun).
                    if *connecting {
                        return Attempt::Done(0);
                    }
                    *connecting = true;
                    return Attempt::Wait(fd, Dir::Write);
                }
                Io::Recv { fd, buf, len } => (
                    retry(|| libc::recv(fd, buf.cast(), len as usize, libc::MSG_DONTWAIT)),
                    Dir::Read,
                ),
                // Zero-copy sends are io_uring's: epoll always copies.
                Io::Send {
                    fd,
                    buf,
                    len,
                    flags,
                    ..
                } => (
                    retry(|| libc::send(fd, buf.cast(), len as usize, flags | libc::MSG_DONTWAIT)),
                    Dir::Write,
                ),
                Io::SendMsg { fd, msg, flags, .. } => (
                    retry(|| libc::sendmsg(fd, msg, flags as i32 | libc::MSG_DONTWAIT)),
                    Dir::Write,
                ),
                Io::RecvMsg { fd, msg } => (
                    retry(|| libc::recvmsg(fd, msg, libc::MSG_DONTWAIT)),
                    Dir::Read,
                ),
                // A pipe or a socket: offsets do not apply.
                Io::ReadReady { fd, buf, len } | Io::Read { fd, buf, len, .. } => (
                    retry(|| libc::read(fd, buf.cast(), len as usize)),
                    Dir::Read,
                ),
                Io::Write { fd, buf, len, .. } => (
                    retry(|| libc::write(fd, buf.cast(), len as usize)),
                    Dir::Write,
                ),
                _ => unreachable!(),
            }
        };
        if is_would_block(res) {
            Attempt::Wait(fd, dir)
        } else {
            Attempt::Done(res)
        }
    }

    // ------------------------------------------------------------- single-shot

    pub(crate) fn start(&mut self, io: &Io, waker: &Waker) -> Start {
        if let Io::Close { fd } = *io {
            if self.busy.contains_key(&fd) {
                // A cancelled job still uses it: close it when that reports.
                self.close_fd(fd);
                return Start::Done(0, 0);
            }
        }
        if self.goes_to_thread(io) {
            return Start::Submitted(self.start_blocking(io, waker));
        }
        if !spend_budget() {
            // The task has done enough at once: let the others run first.
            let key = self.ops.insert(EOp::Ready {
                io: *io,
                connecting: false,
            });
            waker.wake_by_ref();
            return Start::Submitted(key);
        }
        let mut connecting = false;
        match self.attempt(io, &mut connecting) {
            Attempt::Done(res) => Start::Done(res, 0),
            Attempt::Wait(fd, dir) => {
                let key = self.ops.insert(EOp::Ready {
                    io: *io,
                    connecting,
                });
                self.wait(fd, dir, waker);
                Start::Submitted(key)
            }
        }
    }

    /// `true` for the operations that go to a blocking thread: every file
    /// operation, except reads and writes on pipes and sockets.
    fn goes_to_thread(&mut self, io: &Io) -> bool {
        match *io {
            Io::Read { fd, .. } | Io::Write { fd, .. } => !self.pollable_file(fd),
            Io::Close { fd } => {
                // The number may be reused as soon as the thread closes it.
                self.forget(fd);
                true
            }
            Io::OpenAt { .. }
            | Io::Statx { .. }
            | Io::Fsync { .. }
            | Io::Fallocate { .. }
            | Io::UnlinkAt { .. }
            | Io::MkDirAt { .. }
            | Io::RenameAt { .. } => true,
            Io::Connect { .. }
            | Io::Recv { .. }
            | Io::Send { .. }
            | Io::Writable { .. }
            | Io::SendMsg { .. }
            | Io::RecvMsg { .. }
            | Io::ReadReady { .. } => false,
        }
    }

    fn start_blocking(&mut self, io: &Io, waker: &Waker) -> usize {
        let cancel = Cancel::new();
        let fd = match *io {
            Io::Read { fd, .. }
            | Io::Write { fd, .. }
            | Io::Fsync { fd, .. }
            | Io::Fallocate { fd, .. }
            | Io::Statx { dir: fd, .. } => fd,
            _ => -1,
        };
        if fd >= 0 {
            self.busy.entry(fd).or_insert((0, false)).0 += 1;
        }
        let key = self.ops.insert(EOp::Blocking {
            state: Lifecycle::Waiting(waker.clone()),
            cancel: cancel.clone(),
            fd,
        });
        self.blocking += 1;
        let completions = self.completions.clone();
        let job = SendIo(*io);
        blocking::spawn(Box::new(move || {
            let res = if cancel.is_cancelled() {
                -libc::ECANCELED
            } else {
                job.run()
            };
            completions.push(key, res);
        }));
        key
    }

    pub(crate) fn poll_op(&mut self, key: usize, waker: &Waker) -> Poll<(i32, u32)> {
        let (io, mut connecting) = match self.ops.get_mut(key) {
            Some(EOp::Ready { io, connecting }) => (*io, *connecting),
            Some(EOp::Blocking {
                state: Lifecycle::Done(res),
                ..
            }) => {
                let res = *res;
                self.ops.remove(key);
                return Poll::Ready((res, 0));
            }
            Some(EOp::Blocking {
                state: Lifecycle::Waiting(w),
                ..
            }) => {
                if !w.will_wake(waker) {
                    *w = waker.clone();
                }
                return Poll::Pending;
            }
            _ => panic!("giong: operation polled in an invalid state"),
        };
        if !spend_budget() {
            waker.wake_by_ref();
            return Poll::Pending;
        }
        match self.attempt(&io, &mut connecting) {
            Attempt::Done(res) => {
                self.ops.remove(key);
                Poll::Ready((res, 0))
            }
            Attempt::Wait(fd, dir) => {
                if let Some(EOp::Ready { connecting: c, .. }) = self.ops.get_mut(key) {
                    *c = connecting;
                }
                self.wait(fd, dir, waker);
                Poll::Pending
            }
        }
    }

    pub(crate) fn cancel_op(
        &mut self,
        key: usize,
        data: Option<Box<dyn Any>>,
        closes_result: bool,
    ) {
        match self.ops.get_mut(key) {
            Some(EOp::Blocking {
                state: state @ Lifecycle::Waiting(_),
                cancel,
                ..
            }) => {
                // The thread may be using the resources: keep them until it
                // reports. A job that has not started yet skips its call.
                cancel.cancel();
                *state = Lifecycle::Ignored {
                    data,
                    closes_result,
                };
            }
            Some(&mut EOp::Blocking {
                state: Lifecycle::Done(res),
                ..
            }) => {
                // Nothing is using the resources any more.
                self.ops.remove(key);
                if let Some(data) = data {
                    self.graveyard.push(data);
                }
                // An open that finished before its future was dropped.
                if closes_result && res >= 0 {
                    self.close_fd(res);
                }
            }
            Some(EOp::Ready { .. }) => {
                // Nothing is using the resources any more.
                self.ops.remove(key);
                if let Some(data) = data {
                    self.graveyard.push(data);
                }
            }
            _ => {
                // Unknown state: never free memory that may still be used.
                mem::forget(data);
            }
        }
    }

    // --------------------------------------------------------------- multishot

    pub(crate) fn new_multi(&mut self, _kind: MultiKind) -> usize {
        self.ops.insert(EOp::Multi { io: None })
    }

    pub(crate) fn arm_multi(&mut self, key: usize, new: MultiIo) {
        match self.ops.get_mut(key) {
            Some(EOp::Multi { io: io @ None }) => *io = Some(new),
            _ => panic!("giong: arming an invalid multishot operation"),
        }
    }

    pub(crate) fn poll_multi(&mut self, key: usize, waker: &Waker) -> MultiPoll {
        let io = match self.ops.get(key) {
            Some(EOp::Multi { io: Some(io) }) => *io,
            Some(EOp::Multi { io: None }) => return MultiPoll::NeedArm,
            _ => panic!("giong: polling an invalid multishot operation"),
        };
        if !spend_budget() {
            waker.wake_by_ref();
            return MultiPoll::Pending;
        }
        let fd = match io {
            MultiIo::Accept { fd } | MultiIo::Recv { fd } | MultiIo::RecvMsg { fd, .. } => fd,
        };
        if let Err(e) = self.register(fd) {
            self.disarm(key);
            return MultiPoll::Item(e);
        }
        let outcome = match io {
            MultiIo::Accept { fd } => self.accept(fd),
            MultiIo::Recv { fd } => self.recv_into_pool(fd),
            MultiIo::RecvMsg { fd, hdr } => self.recvmsg_into_pool(fd, hdr),
        };
        match outcome {
            Some(Ok((res, flags))) => match super::buffer_id(flags) {
                Some(bid) if res > 0 => {
                    let pool = self.pool.clone();
                    MultiPoll::Data(RecvBuf::pooled(pool, bid, 0, res as u32))
                }
                _ => MultiPoll::Item(res),
            },
            Some(Err(res)) if is_would_block(res) => {
                self.wait(fd, Dir::Read, waker);
                MultiPoll::Pending
            }
            Some(Err(res)) => {
                // Like a multishot that ended with an error: the next poll
                // arms it again.
                self.disarm(key);
                MultiPoll::Item(res)
            }
            None => {
                // No pool buffer: the caller waits for one, then arms again.
                self.disarm(key);
                MultiPoll::NeedArm
            }
        }
    }

    fn disarm(&mut self, key: usize) {
        if let Some(EOp::Multi { io }) = self.ops.get_mut(key) {
            *io = None;
        }
    }

    fn accept(&mut self, fd: RawFd) -> Option<Result<(i32, u32), i32>> {
        let res = retry(|| sys::accept(fd));
        Some(if res >= 0 { Ok((res, 0)) } else { Err(res) })
    }

    fn recv_into_pool(&mut self, fd: RawFd) -> Option<Result<(i32, u32), i32>> {
        let (bid, ptr, len) = self.pool.next_free()?;
        // SAFETY: `ptr` is a free pool buffer of `len` bytes.
        let res = retry(|| unsafe { libc::recv(fd, ptr.cast(), len, libc::MSG_DONTWAIT) });
        Some(match res {
            n if n > 0 => {
                self.pool.take_next();
                Ok((n, pooled_flags(bid)))
            }
            // End of stream: no buffer. Every later poll reports it again.
            0 => Ok((0, 0)),
            e => Err(e),
        })
    }

    /// One datagram into a pool buffer, laid out like io_uring's multishot
    /// `recvmsg`: header, address field, payload (see [`MultiIo::RecvMsg`]).
    fn recvmsg_into_pool(
        &mut self,
        fd: RawFd,
        hdr: *const libc::msghdr,
    ) -> Option<Result<(i32, u32), i32>> {
        const OUT_HEADER: usize = 16;
        let (bid, ptr, len) = self.pool.next_free()?;
        // SAFETY: `hdr` points at a `'static` template (see `MultiIo`).
        // `msg_controllen` is a `usize` on glibc and a `u32` on musl.
        #[allow(clippy::unnecessary_cast)]
        let (name_space, control_space) =
            unsafe { ((*hdr).msg_namelen as usize, (*hdr).msg_controllen as usize) };
        let payload_at = OUT_HEADER + name_space + control_space;
        if len < payload_at {
            return Some(Err(-libc::ENOBUFS));
        }
        // SAFETY: every pointer in this block stays inside the free pool
        // buffer of `len` bytes; all-zero is a valid `msghdr`.
        unsafe {
            let mut iov = libc::iovec {
                iov_base: ptr.add(payload_at).cast(),
                iov_len: len - payload_at,
            };
            let mut msg: libc::msghdr = mem::zeroed();
            msg.msg_name = ptr.add(OUT_HEADER).cast();
            msg.msg_namelen = name_space as libc::socklen_t;
            if control_space > 0 {
                msg.msg_control = ptr.add(OUT_HEADER + name_space).cast();
                msg.msg_controllen = control_space as _;
            }
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            // MSG_TRUNC: the result is the datagram's real length, so a cut
            // off datagram is reported as such.
            let res = retry(|| {
                libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT | libc::MSG_TRUNC) as isize
            });
            if res < 0 {
                return Some(Err(res));
            }
            let header = [
                msg.msg_namelen as u32,
                msg.msg_controllen as u32,
                res as u32,
                msg.msg_flags as u32,
            ];
            for (i, field) in header.iter().enumerate() {
                ptr.add(i * 4).cast::<u32>().write_unaligned(*field);
            }
            self.pool.take_next();
            let total = payload_at + (res as usize).min(len - payload_at);
            Some(Ok((total as i32, pooled_flags(bid))))
        }
    }

    pub(crate) fn drop_multi(&mut self, key: usize) {
        if matches!(self.ops.get(key), Some(EOp::Multi { .. })) {
            self.ops.remove(key);
        }
    }

    // -------------------------------------------------------------------- park

    /// `true` if something can be collected without waiting.
    pub(crate) fn has_completions(&mut self) -> bool {
        if self.ready > 0 || self.completions.any() {
            return true;
        }
        self.ready = self.wait_events(Some(Duration::ZERO));
        self.ready > 0
    }

    pub(crate) fn park(&mut self, mode: Park) -> Vec<Box<dyn Any>> {
        let n = if self.ready > 0 {
            mem::take(&mut self.ready)
        } else {
            let timeout = match mode {
                _ if self.completions.any() => Some(Duration::ZERO),
                Park::Poll => Some(Duration::ZERO),
                Park::Wait(timeout) => timeout,
            };
            self.wait_events(timeout)
        };
        self.dispatch(n);
        self.collect_blocking();
        mem::take(&mut self.graveyard)
    }

    /// Waits for events; `None` waits without a time limit.
    fn wait_events(&mut self, timeout: Option<Duration>) -> usize {
        self.poller.wait(timeout)
    }

    /// Wakes the tasks waiting for the descriptors in the first `n` events.
    fn dispatch(&mut self, n: usize) {
        for i in 0..n {
            let (token, readable, writable) = self.poller.event(i);
            match token {
                WAKE => sys::drain(self.wake_fd),
                DONE => {} // collected below
                token => {
                    let fd = token as u32 as usize;
                    let gen = (token >> 32) as u32;
                    let Some(Some(Slot::Pollable(reg))) = self.fds.get_mut(fd) else {
                        continue;
                    };
                    if reg.gen != gen {
                        continue;
                    }
                    if readable {
                        for waker in reg.readers.drain(..) {
                            waker.wake();
                        }
                    }
                    if writable {
                        for waker in reg.writers.drain(..) {
                            waker.wake();
                        }
                    }
                }
            }
        }
    }

    /// Applies the results the blocking threads have reported.
    fn collect_blocking(&mut self) {
        if !self.completions.any() {
            return;
        }
        let mut done = mem::take(&mut self.done);
        self.completions.take(&mut done);
        for (key, res) in done.drain(..) {
            self.blocking -= 1;
            let mut used = -1;
            // A descriptor an ignored open produced: nobody will take it.
            let mut orphan_fd = None;
            let remove = match self.ops.get_mut(key) {
                Some(EOp::Blocking { state, fd, .. }) => {
                    used = *fd;
                    match mem::replace(state, Lifecycle::Done(res)) {
                        Lifecycle::Waiting(waker) => {
                            waker.wake();
                            false
                        }
                        Lifecycle::Ignored {
                            data,
                            closes_result,
                        } => {
                            if let Some(data) = data {
                                self.graveyard.push(data);
                            }
                            if closes_result && res >= 0 {
                                orphan_fd = Some(res);
                            }
                            true
                        }
                        Lifecycle::Done(_) => false,
                    }
                }
                _ => false,
            };
            if remove {
                self.ops.remove(key);
            }
            self.release(used);
            if let Some(fd) = orphan_fd {
                self.close_fd(fd);
            }
        }
        self.done = done;
    }

    // ---------------------------------------------------------------- teardown

    /// Cancels what is left and waits up to a second for the blocking
    /// threads. `false` if one is still running: the caller leaks the
    /// driver's memory rather than free what the thread may write to.
    pub(crate) fn shutdown(&mut self) -> bool {
        for op in self.ops.iter_mut() {
            if let EOp::Blocking { cancel, .. } = op {
                cancel.cancel();
            }
        }
        for _ in 0..100 {
            self.collect_blocking();
            if self.blocking == 0 {
                return true;
            }
            self.wait_events(Some(Duration::from_millis(10)));
        }
        self.collect_blocking();
        self.blocking == 0
    }

    pub(crate) fn take_graveyard(&mut self) -> Vec<Box<dyn Any>> {
        mem::take(&mut self.graveyard)
    }

    /// Blocking jobs that have not reported yet.
    pub(crate) fn inflight(&self) -> usize {
        self.blocking
    }

    pub(crate) fn tracked_ops(&self) -> usize {
        self.ops.len()
    }
}

impl Attempt {
    /// Turns "still connecting" into a wait for writability.
    fn or_wait(self, fd: RawFd, dir: Dir) -> Attempt {
        match self {
            Attempt::Done(res) if res == -libc::ENOTCONN => Attempt::Wait(fd, dir),
            other => other,
        }
    }
}

/// The outcome of a connect in progress: `0` once connected, `-errno` if it
/// failed, `-ENOTCONN` while it is still going on.
///
/// # Safety
///
/// `fd` must be an open socket.
unsafe fn connect_result(fd: RawFd) -> i32 {
    let mut err: libc::c_int = 0;
    let mut len = mem::size_of::<libc::c_int>() as libc::socklen_t;
    if libc::getsockopt(
        fd,
        libc::SOL_SOCKET,
        libc::SO_ERROR,
        (&mut err as *mut libc::c_int).cast(),
        &mut len,
    ) < 0
    {
        return neg_errno();
    }
    if err != 0 {
        return -err;
    }
    let mut addr: libc::sockaddr_storage = mem::zeroed();
    let mut len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    if libc::getpeername(
        fd,
        (&mut addr as *mut libc::sockaddr_storage).cast(),
        &mut len,
    ) < 0
    {
        return neg_errno();
    }
    0
}

/// Completion flags for data in pool buffer `bid`.
fn pooled_flags(bid: u16) -> u32 {
    F_BUFFER | (u32::from(bid) << BUFFER_SHIFT) | F_MORE
}

/// Puts `fd` in non-blocking mode.
#[cfg(not(miri))]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let mut on: libc::c_int = 1;
    // SAFETY: FIONBIO reads an int from a valid pointer.
    if unsafe { libc::ioctl(fd, libc::FIONBIO, &mut on) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Puts `fd` in non-blocking mode. Miri has no `ioctl(FIONBIO)`, but has
/// `fcntl` for pipes and sockets.
#[cfg(miri)]
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain syscalls on a descriptor; the results are checked.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
