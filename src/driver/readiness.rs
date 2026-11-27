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
//! budget of [`BUDGET`](super::budget::BUDGET) immediate operations per task poll prevents that:
//! once it is spent, operations report "pending" and wake their task again,
//! which puts it at the back of the run queue.

use std::any::Any;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::mem;
use std::os::fd::RawFd;
use std::ptr;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

use super::blocking::{self, Cancel, Completions};
use super::budget::spend_budget;
use super::{Io, MultiIo, MultiKind, MultiPoll, Park, Start, BUFFER_SHIFT, F_BUFFER, F_MORE};
use crate::buf::{BufPool, RecvBuf};
use crate::slab::Slab;
use crate::sys;

#[cfg(any(target_os = "macos", giong_inject_enobufs))]
mod nobufs;

/// What the readiness driver needs from the kernel's event queue (epoll,
/// kqueue). Tokens are 64 bits: [`WAKE`] and [`DONE`] for the two bells,
/// otherwise a descriptor number in the lower half and a generation in the
/// upper half.
pub(crate) trait Poller: Sized {
    /// A look that cannot wait (`Park::Poll`) is skipped while no
    /// descriptor is registered: it could only find the two bells, whose
    /// news keeps (the blocking threads' reports are read from memory, and
    /// a wake-up stays pending for the next wait). Set where a look costs
    /// far more than the polls between looks: on macOS each `kevent` is an
    /// involuntary context switch (about 20 µs on the CI runners), while an
    /// `epoll_wait` that finds nothing is cheap, so epoll keeps looking.
    const SKIP_EMPTY_POLL: bool;
    /// Creates the event queue.
    fn new() -> io::Result<Self>;
    /// Times short waits exactly, without the kernel's timer slack (see
    /// `Builder::precise_timers`). Only kqueue has that slack.
    fn set_precise_timers(&mut self, _enable: bool) {}
    /// Makes the worker's wake-up: ringing it from any thread makes
    /// [`wait`](Self::wait) report an event with the token [`WAKE`], and
    /// [`WakeHandle::seen`](super::WakeHandle) resets it. Called once.
    fn waker(&self) -> io::Result<super::WakeHandle>;
    /// Watches a bell for readability, level-triggered.
    fn watch(&self, fd: RawFd, token: u64) -> io::Result<()>;
    /// Registers a descriptor for reading and writing, edge-triggered;
    /// `-errno` on failure. Registering a number again replaces the old
    /// registration.
    fn register(&self, fd: RawFd, token: u64) -> Result<(), i32>;
    /// Adds back a descriptor that [`unregister`](Self::unregister) took
    /// out, as [`register`](Self::register) registers one; readiness that
    /// is already there is reported. 0, or `-errno`.
    fn rearm(&self, fd: RawFd, token: u64) -> i32;
    /// Takes a registered descriptor out of the set; `false` if the kernel
    /// refused.
    fn unregister(&self, fd: RawFd) -> bool;
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

/// Token of the worker's wake-up bell (cross-thread wake-ups).
pub(crate) const WAKE: u64 = u64::MAX;
/// Token of the bell the blocking threads ring.
pub(crate) const DONE: u64 = u64::MAX - 1;

/// Datagrams a socket receives while in the poller's set before it leaves
/// it, once no task waits on it (see [`Registration::armed`]): a socket that
/// stays busy pays for no wake-ups, and one that is not pays at most two
/// poller calls (`epoll_ctl` on Linux) per this many.
const QUIET_AFTER: u32 = 64;

/// `list.push(w)`, inlined: `Vec::push` stayed a call in
/// [`Readiness::wait`], which every stream receive that finds nothing runs,
/// and cost a TCP round trip 1% more instructions. The list nearly always
/// has room (it keeps its capacity when drained).
#[inline(always)]
fn push_inline(list: &mut Vec<Waker>, w: Waker) {
    if list.len() == list.capacity() {
        list.reserve(1);
    }
    let len = list.len();
    // SAFETY: there is room for one more element (reserved above); it is
    // written before the length covers it.
    unsafe {
        list.as_mut_ptr().add(len).write(w);
        list.set_len(len + 1);
    }
}

/// Adds `fd`, a datagram socket that left the poller's set while busy, back
/// to it before a task waits on it. Adding reports readiness that is already
/// there, so nothing is missed while it was out. `false` (and `waker` woken,
/// to try again on the next turn rather than hang) if it cannot be added.
#[cold]
#[inline(never)]
fn rearm<P: Poller>(poller: &P, reg: &mut Registration, fd: RawFd, waker: &Waker) -> bool {
    if poller.rearm(fd, (u64::from(reg.gen) << 32) | u64::from(fd as u32)) < 0 {
        waker.wake_by_ref();
        return false;
    }
    reg.armed = true;
    reg.datagrams = 0;
    true
}

/// Takes busy datagram socket `fd` out of the poller's set if no task waits
/// on it (see [`Registration::armed`]).
/// `true` once it is out (or cannot be taken out).
fn leave_set<P: Poller>(poller: &P, reg: &mut Registration, fd: RawFd) -> bool {
    if !reg.readers.is_empty() || !reg.writers.is_empty() {
        return false;
    }
    if poller.unregister(fd) {
        reg.armed = false;
    }
    true
}

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
    /// In the poller's set. A busy datagram socket leaves it (see
    /// [`QUIET_AFTER`]) and comes back when a task has to wait for it.
    armed: bool,
    /// Datagrams received since the descriptor was last added to the set.
    datagrams: u32,
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
    /// A multishot operation, armed once `io` is set. `batch`: datagrams
    /// the next receive asks for; `streak`: receives in a row that found a
    /// datagram (see [`Readiness::set_batch`]).
    Multi {
        io: Option<MultiIo>,
        batch: u8,
        streak: u8,
    },
}

/// Datagrams one `recvmmsg` takes at most.
const RECV_BATCH: usize = 16;

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
            if offset == u64::MAX {
                // The file's own position, as io_uring reads at offset -1.
                return retry(|| libc::read(fd, buf.cast(), len as usize));
            }
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
            if offset == u64::MAX {
                return retry(|| libc::write(fd, buf.cast(), len as usize));
            }
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
        Io::OpenBeneath { dir, path, how } => retry(|| sys::open_beneath(dir, path, &*how)),
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
        | Io::SendTo { .. }
        | Io::RecvMsg { .. }
        | Io::Readable { .. }
        | Io::ReadReady { .. } => unreachable!("socket operations never block a thread"),
    }
}

/// A datagram of an earlier `recvmmsg` (pool buffer, length written), or
/// the error it met after its datagrams.
type Ahead = Result<(u16, u32), i32>;

/// The readiness driver over poller `P`.
pub(crate) struct Readiness<P: Poller> {
    poller: P,
    pool: Rc<BufPool>,
    /// The worker's wake-up, made by the poller and rung by threads that
    /// wake this worker.
    wake: super::WakeHandle,
    completions: Arc<Completions>,
    ops: Slab<EOp>,
    /// By multishot operation: datagrams received in the same `recvmmsg` as
    /// one already handed out, as pool buffers and the length of what was
    /// written into each, then the error (a negative `errno`) that the
    /// receive met after them, if any (see `sys::take_recv_error`). Apart
    /// from the operations, which every stream has, so that they stay small.
    ahead: HashMap<usize, VecDeque<Ahead>>,
    /// Busy datagram sockets (descriptor, generation) to take out of the
    /// poller's set once no task waits on them (see [`QUIET_AFTER`]).
    quieting: Vec<(RawFd, u32)>,
    /// What is known about each descriptor, indexed by its number.
    fds: Vec<Option<Slot>>,
    /// How many of `fds` are [`Slot::Pollable`].
    pollable: usize,
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
    /// Sends and connects waiting out a shortage of network buffers.
    #[cfg(any(target_os = "macos", giong_inject_enobufs))]
    retries: nobufs::Retries,
}

impl<P: Poller> Readiness<P> {
    /// See [`Poller::set_precise_timers`].
    #[cfg_attr(not(any(target_os = "macos", giong_libkqueue)), allow(dead_code))]
    pub(crate) fn set_precise_timers(&mut self, enable: bool) {
        self.poller.set_precise_timers(enable);
    }

    pub(crate) fn new(pool: Rc<BufPool>) -> io::Result<Self> {
        let poller = P::new()?;
        let wake = poller.waker()?;
        let completions = Completions::new()?;
        let driver = Self {
            poller,
            pool,
            wake,
            completions,
            ops: Slab::new(),
            fds: Vec::new(),
            pollable: 0,
            next_gen: 1,
            ready: 0,
            done: Vec::new(),
            blocking: 0,
            busy: HashMap::new(),
            ahead: HashMap::new(),
            quieting: Vec::new(),
            graveyard: Vec::new(),
            #[cfg(any(target_os = "macos", giong_inject_enobufs))]
            retries: nobufs::Retries::default(),
        };
        driver.poller.watch(driver.completions.fd(), DONE)?;
        Ok(driver)
    }

    pub(crate) fn waker(&self) -> super::WakeHandle {
        self.wake.clone()
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
                armed: true,
                datagrams: 0,
            }),
        );
        Ok(())
    }

    fn set_slot(&mut self, i: usize, slot: Slot) {
        if self.fds.len() <= i {
            self.fds.resize_with(i + 1, || None);
        }
        self.pollable += usize::from(matches!(slot, Slot::Pollable(_)));
        if let Some(Slot::Pollable(_)) = self.fds[i].replace(slot) {
            self.pollable -= 1;
        }
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

    /// Sends `fd`'s reads and writes to the blocking threads from now on,
    /// leaving its flags alone (see `Driver::keep_blocking`).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn keep_blocking(&mut self, fd: RawFd) {
        if let Ok(i) = usize::try_from(fd) {
            if !matches!(self.fds.get(i), Some(Some(Slot::Pollable(_)))) {
                self.set_slot(i, Slot::Blocking);
            }
        }
    }

    /// Forgets `fd`, which leaves this driver's care without being closed,
    /// and takes it out of the poller's set.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn release_fd(&mut self, fd: RawFd) {
        if let Ok(i) = usize::try_from(fd) {
            if matches!(self.fds.get(i), Some(Some(Slot::Pollable(_)))) {
                self.poller.unregister(fd);
            }
        }
        self.forget(fd);
    }

    /// Forgets what is known about `fd`, which is being closed.
    fn forget(&mut self, fd: RawFd) {
        if let Some(slot) = usize::try_from(fd).ok().and_then(|i| self.fds.get_mut(i)) {
            if let Some(Slot::Pollable(_)) = slot.take() {
                self.pollable -= 1;
            }
        }
    }

    /// Wakes `waker` when `fd` becomes ready in direction `dir`. Inlined:
    /// every stream receive that finds nothing comes here.
    #[inline(always)]
    fn wait(&mut self, fd: RawFd, dir: Dir, waker: &Waker) {
        let poller = &self.poller;
        let Some(Some(Slot::Pollable(reg))) = self.fds.get_mut(fd as usize) else {
            // Not registered (cannot happen after a successful attempt):
            // try again on the next turn rather than hang.
            waker.wake_by_ref();
            return;
        };
        if !reg.armed && !rearm(poller, reg, fd, waker) {
            return;
        }
        let list = match dir {
            Dir::Read => &mut reg.readers,
            Dir::Write => &mut reg.writers,
        };
        if !list.iter().any(|w| w.will_wake(waker)) {
            push_inline(list, waker.clone());
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
            | Io::SendTo { fd, .. }
            | Io::RecvMsg { fd, .. }
            | Io::Readable { fd }
            | Io::ReadReady { fd, .. }
            | Io::Read { fd, .. }
            | Io::Write { fd, .. } => fd,
            _ => unreachable!("file operations go to the blocking threads"),
        };
        if let Err(e) = self.register(fd) {
            return Attempt::Done(e);
        }
        #[cfg(giong_inject_enobufs)]
        if let Some(res) = nobufs::inject::refused(io, *connecting) {
            return Attempt::Done(res);
        }
        // SAFETY: for every call below, the pointers come from `Op`
        // resources, valid while the operation exists; the descriptor is
        // non-blocking.
        let (res, dir) = unsafe {
            match *io {
                Io::Connect { fd, addr, len } => {
                    if *connecting {
                        let res = connect_result(fd);
                        #[cfg(giong_inject_enobufs)]
                        let res = nobufs::inject::connected(res, connecting);
                        return Attempt::Done(res).or_wait(fd, Dir::Write);
                    }
                    let res = match libc::connect(fd, addr, len) {
                        0 => 0,
                        _ => neg_errno(),
                    };
                    // Tried again after `ENOBUFS`, the call may find the
                    // first one under way after all.
                    #[cfg(any(target_os = "macos", giong_inject_enobufs))]
                    let res = nobufs::connect_again(res);
                    #[cfg(giong_inject_enobufs)]
                    let res = nobufs::inject::connected(res, connecting);
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
                Io::SendTo {
                    fd,
                    buf,
                    len,
                    addr,
                    addr_len,
                } => (
                    retry(|| {
                        libc::sendto(
                            fd,
                            buf.cast(),
                            len as usize,
                            sys::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                            addr,
                            addr_len,
                        )
                    }),
                    Dir::Write,
                ),
                Io::RecvMsg { fd, msg, flags } => (
                    retry(|| libc::recvmsg(fd, msg, flags | libc::MSG_DONTWAIT)),
                    Dir::Read,
                ),
                Io::Readable { fd } => {
                    // As `Writable`: wait for the next "readable" edge
                    // (`connecting` marks a wait that has begun). A
                    // descriptor that is readable already reports it at
                    // once, since registering it raises the edge.
                    if *connecting {
                        return Attempt::Done(0);
                    }
                    let mut pfd = libc::pollfd {
                        fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    if libc::poll(&mut pfd, 1, 0) > 0 {
                        return Attempt::Done(0);
                    }
                    *connecting = true;
                    return Attempt::Wait(fd, Dir::Read);
                }
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
            #[cfg(any(target_os = "macos", giong_inject_enobufs))]
            Attempt::Done(res) if nobufs::retries(io, connecting, res) => {
                let key = self.ops.insert(EOp::Ready {
                    io: *io,
                    connecting,
                });
                // The first step is always taken.
                self.retries.schedule(key, waker);
                Start::Submitted(key)
            }
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
            | Io::OpenBeneath { .. }
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
            | Io::SendTo { .. }
            | Io::RecvMsg { .. }
            | Io::Readable { .. }
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
            | Io::Statx { dir: fd, .. }
            | Io::OpenBeneath { dir: fd, .. } => fd,
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
        // One comparison while no operation waits for network buffers.
        #[cfg(any(target_os = "macos", giong_inject_enobufs))]
        if !self.retries.is_empty() && self.retries.waiting(key, waker) {
            return Poll::Pending;
        }
        if !spend_budget() {
            waker.wake_by_ref();
            return Poll::Pending;
        }
        match self.attempt(&io, &mut connecting) {
            Attempt::Done(res) => {
                #[cfg(any(target_os = "macos", giong_inject_enobufs))]
                if self.retry_later(key, &io, connecting, res, waker) {
                    return Poll::Pending;
                }
                self.ops.remove(key);
                Poll::Ready((res, 0))
            }
            Attempt::Wait(fd, dir) => {
                if let Some(EOp::Ready { connecting: c, .. }) = self.ops.get_mut(key) {
                    *c = connecting;
                }
                #[cfg(any(target_os = "macos", giong_inject_enobufs))]
                self.retries.forget(key);
                self.wait(fd, dir, waker);
                Poll::Pending
            }
        }
    }

    /// `true` if operation `key`, whose attempt came to `res`, waits for
    /// network buffers and is tried again later (see [`nobufs`]); otherwise
    /// it no longer waits for them.
    #[cfg(any(target_os = "macos", giong_inject_enobufs))]
    #[cold]
    fn retry_later(
        &mut self,
        key: usize,
        io: &Io,
        connecting: bool,
        res: i32,
        waker: &Waker,
    ) -> bool {
        if nobufs::retries(io, connecting, res) && self.retries.schedule(key, waker) {
            return true;
        }
        self.retries.forget(key);
        false
    }

    pub(crate) fn cancel_op(
        &mut self,
        key: usize,
        data: Option<Box<dyn Any>>,
        closes_result: bool,
        finish: bool,
    ) {
        match self.ops.get_mut(key) {
            Some(EOp::Blocking {
                state: state @ Lifecycle::Waiting(_),
                cancel,
                ..
            }) => {
                // The thread may be using the resources: keep them until it
                // reports. A job that has not started yet skips its call,
                // unless it must run to the end (a close).
                if !finish {
                    cancel.cancel();
                }
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
                #[cfg(any(target_os = "macos", giong_inject_enobufs))]
                self.retries.forget(key);
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
        self.ops.insert(EOp::Multi {
            io: None,
            batch: 1,
            streak: 0,
        })
    }

    pub(crate) fn arm_multi(&mut self, key: usize, new: MultiIo) {
        match self.ops.get_mut(key) {
            Some(EOp::Multi { io: io @ None, .. }) => *io = Some(new),
            _ => panic!("giong: arming an invalid multishot operation"),
        }
    }

    pub(crate) fn poll_multi(&mut self, key: usize, waker: &Waker) -> MultiPoll {
        let io = match self.ops.get(key) {
            Some(EOp::Multi { io: Some(io), .. }) => *io,
            Some(EOp::Multi { io: None, .. }) => return MultiPoll::NeedArm,
            _ => panic!("giong: polling an invalid multishot operation"),
        };
        let (fd, hdr) = match io {
            MultiIo::Accept { fd } | MultiIo::Recv { fd, .. } => (fd, None),
            MultiIo::RecvMsg { fd, hdr } => (fd, Some(hdr)),
        };
        if let Some(hdr) = hdr {
            return self.poll_datagram(key, fd, hdr, waker);
        }
        if !spend_budget() {
            waker.wake_by_ref();
            return MultiPoll::Pending;
        }
        if let Err(e) = self.register(fd) {
            self.disarm(key);
            return MultiPoll::Item(e);
        }
        let outcome = match io {
            MultiIo::Accept { fd } => self.accept(fd),
            _ => self.recv_into_pool(fd),
        };
        self.received(key, fd, outcome, waker)
    }

    /// [`poll_multi`](Self::poll_multi) for a datagram socket's receive:
    /// first what an earlier `recvmmsg` brought. Out of line, so that the
    /// path every stream takes stays as small as it was.
    #[inline(never)]
    fn poll_datagram(
        &mut self,
        key: usize,
        fd: RawFd,
        hdr: *const libc::msghdr,
        waker: &Waker,
    ) -> MultiPoll {
        match self.ahead.get_mut(&key).and_then(VecDeque::pop_front) {
            // Received earlier, in the same system call as another.
            Some(Ok((bid, len))) => {
                let pool = self.pool.clone();
                return MultiPoll::Data(RecvBuf::pooled(pool, bid, 0, len));
            }
            // What that system call met after them.
            Some(Err(res)) => {
                self.disarm(key);
                return MultiPoll::Item(res);
            }
            None => {}
        }
        if !spend_budget() {
            waker.wake_by_ref();
            return MultiPoll::Pending;
        }
        if let Err(e) = self.register(fd) {
            self.disarm(key);
            return MultiPoll::Item(e);
        }
        let outcome = self.recvmsg_into_pool(key, fd, hdr);
        self.received(key, fd, outcome, waker)
    }

    /// What a receive on multishot operation `key` (on `fd`) came to.
    #[inline(always)]
    fn received(
        &mut self,
        key: usize,
        fd: RawFd,
        outcome: Option<Result<(i32, u32), i32>>,
        waker: &Waker,
    ) -> MultiPoll {
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
        if let Some(EOp::Multi { io, .. }) = self.ops.get_mut(key) {
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
        key: usize,
        fd: RawFd,
        hdr: *const libc::msghdr,
    ) -> Option<Result<(i32, u32), i32>> {
        const OUT_HEADER: usize = 16;
        // SAFETY: `hdr` points at a `'static` template (see `MultiIo`).
        #[allow(clippy::unnecessary_cast)]
        let no_control = unsafe { (*hdr).msg_controllen as usize == 0 };
        // Several datagrams in one system call, like io_uring's multishot
        // receive, while at least half the pool stays free for others. The
        // number asked for doubles while each receive comes back full and
        // drops to one when the socket runs dry, so that a socket that gets
        // one datagram at a time does not set up a batch for each.
        let batch = match self.ops.get(key) {
            Some(EOp::Multi { batch, .. }) => usize::from(*batch),
            _ => 1,
        };
        let room = self
            .pool
            .free()
            .saturating_sub(usize::from(self.pool.count()) / 2)
            .min(batch);
        if no_control && room > 1 {
            // SAFETY: as above.
            let name_space = unsafe { (*hdr).msg_namelen as usize };
            let out = self.recvmmsg_into_pool(key, fd, name_space, room);
            let got = match out {
                Some(Ok(_)) => {
                    1 + self
                        .ahead
                        .get(&key)
                        .map_or(0, |a| a.iter().filter(|d| d.is_ok()).count())
                }
                _ => 0,
            };
            self.set_batch(key, room, got);
            return out;
        }
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
            // RECV_REAL_LEN (MSG_TRUNC on Linux): the result is the
            // datagram's real length, so a cut off datagram is reported as
            // such. Elsewhere only `recvmsg` says that one was cut off.
            let res = if control_space == 0 && sys::RECVFROM_REPORTS_TRUNCATION {
                // No control data wanted: `recvfrom`, which spares the kernel
                // copying a message header and an iovec array in and out.
                let res = retry(|| {
                    libc::recvfrom(
                        fd,
                        iov.iov_base,
                        iov.iov_len,
                        libc::MSG_DONTWAIT | sys::RECV_REAL_LEN,
                        msg.msg_name.cast(),
                        &mut msg.msg_namelen,
                    ) as isize
                });
                if res as usize > iov.iov_len {
                    msg.msg_flags = libc::MSG_TRUNC;
                }
                res
            } else {
                retry(|| {
                    libc::recvmsg(fd, &mut msg, libc::MSG_DONTWAIT | sys::RECV_REAL_LEN) as isize
                })
            };
            if no_control {
                self.set_batch(key, 1, usize::from(res >= 0));
            }
            if res >= 0 {
                self.note_datagrams(fd, 1);
            }
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

    /// Up to `count` datagrams into pool buffers with one `recvmmsg`, each
    /// laid out as [`recvmsg_into_pool`](Self::recvmsg_into_pool) lays out
    /// one. The first is the result; the others wait in the operation's
    /// `ahead` queue for the next polls.
    fn recvmmsg_into_pool(
        &mut self,
        key: usize,
        fd: RawFd,
        name_space: usize,
        count: usize,
    ) -> Option<Result<(i32, u32), i32>> {
        const OUT_HEADER: usize = 16;
        let payload_at = OUT_HEADER + name_space;
        let mut bufs = [(0u16, ptr::null_mut::<u8>(), 0usize); RECV_BATCH];
        for i in 0..count {
            // `count` is at most the free buffers, so this cannot run out;
            // if it did, give back what was taken.
            let Some(b) = self.pool.next_free() else {
                for &(bid, ..) in &bufs[..i] {
                    self.pool.release(bid);
                }
                return None;
            };
            bufs[i] = b;
            self.pool.take_next();
        }
        let bufs = &bufs[..count];
        if bufs[0].2 < payload_at {
            for &(bid, ..) in bufs {
                self.pool.release(bid);
            }
            return Some(Err(-libc::ENOBUFS));
        }
        // SAFETY: all-zero is valid for these plain C structs.
        let mut iov: [libc::iovec; RECV_BATCH] = unsafe { mem::zeroed() };
        // SAFETY: as above.
        let mut msgs: [sys::MMsgHdr; RECV_BATCH] = unsafe { mem::zeroed() };
        for (i, &(_, p, len)) in bufs.iter().enumerate() {
            // SAFETY: every pointer stays inside its pool buffer of `len`
            // bytes, which this driver holds until it is handed out or
            // released.
            unsafe {
                iov[i].iov_base = p.add(payload_at).cast();
                iov[i].iov_len = len - payload_at;
                msgs[i].msg_hdr.msg_name = p.add(OUT_HEADER).cast();
            }
            msgs[i].msg_hdr.msg_namelen = name_space as libc::socklen_t;
            msgs[i].msg_hdr.msg_iov = &mut iov[i];
            msgs[i].msg_hdr.msg_iovlen = 1;
        }
        // SAFETY: `count` headers pointing at the buffers above.
        // RECV_REAL_LEN: each length is the datagram's real one (Linux).
        let n = retry(|| unsafe {
            sys::recvmmsg(
                fd,
                msgs.as_mut_ptr(),
                count,
                libc::MSG_DONTWAIT | sys::RECV_REAL_LEN,
            )
        });
        let got = n.max(0) as usize;
        self.note_datagrams(fd, got);
        for &(bid, ..) in &bufs[got..] {
            self.pool.release(bid);
        }
        if n <= 0 {
            return Some(Err(if n == 0 { -libc::EAGAIN } else { n }));
        }
        let mut first = None;
        for (i, &(bid, p, len)) in bufs[..got].iter().enumerate() {
            let m = &msgs[i];
            let res = m.msg_len as usize;
            let mut flags = m.msg_hdr.msg_flags as u32;
            if res > len - payload_at {
                flags |= libc::MSG_TRUNC as u32;
            }
            let header = [m.msg_hdr.msg_namelen as u32, 0, res as u32, flags];
            for (j, field) in header.iter().enumerate() {
                // SAFETY: inside the pool buffer, which holds at least
                // `payload_at` bytes.
                unsafe { p.add(j * 4).cast::<u32>().write_unaligned(*field) };
            }
            let total = (payload_at + res.min(len - payload_at)) as u32;
            if i == 0 {
                first = Some((total as i32, pooled_flags(bid)));
            } else if matches!(self.ops.get(key), Some(EOp::Multi { .. })) {
                self.ahead
                    .entry(key)
                    .or_default()
                    .push_back(Ok((bid, total)));
            } else {
                self.pool.release(bid);
            }
        }
        let mut queued = got > 1;
        if let Some(errno) = sys::take_recv_error() {
            if matches!(self.ops.get(key), Some(EOp::Multi { .. })) {
                self.ahead.entry(key).or_default().push_back(Err(-errno));
                queued = true;
            }
        }
        if queued {
            self.wake_readers(fd);
        }
        first.map(Ok)
    }

    /// Wakes the tasks waiting for datagram socket `fd` to become readable,
    /// after a receive put datagrams in the `ahead` queue. They are out of
    /// the socket, which may have nothing left: no readiness event would
    /// tell the waiting tasks (an edge-triggered event that arrives once the
    /// socket is empty again does not say "readable"). Each woken task takes
    /// one or waits again. All of them, since a task may have stopped
    /// waiting without taking its waker off the list.
    #[cold]
    #[inline(never)]
    fn wake_readers(&mut self, fd: RawFd) {
        if let Some(Some(Slot::Pollable(reg))) = self.fds.get_mut(fd as usize) {
            for waker in reg.readers.drain(..) {
                waker.wake();
            }
        }
    }

    /// After a receive that asked for `asked` datagrams and got `got`: the
    /// next asks for twice as many if this one came back full, else for
    /// what came (at least one). From one, it grows only after two
    /// receives in a row found a datagram, so that a socket that gets one
    /// datagram at a time (and finds nothing on every other try) never sets
    /// up a batch.
    fn set_batch(&mut self, key: usize, asked: usize, got: usize) {
        if let Some(EOp::Multi { batch, streak, .. }) = self.ops.get_mut(key) {
            if got == 0 {
                *batch = 1;
                *streak = 0;
                return;
            }
            *streak = streak.saturating_add(1);
            let next = if got < asked {
                got
            } else if asked > 1 || *streak >= 2 {
                asked * 2
            } else {
                1
            };
            *batch = next.min(RECV_BATCH) as u8;
        }
    }

    /// Counts datagrams received on `fd` towards [`QUIET_AFTER`]. A busy
    /// datagram socket leaves the poller's set until a task has to wait for it
    /// again: in the set, every datagram that arrives and every one sent and
    /// freed wakes the set's wait queue, which costs about as much as the
    /// receive.
    fn note_datagrams(&mut self, fd: RawFd, n: usize) {
        if let Some(Some(Slot::Pollable(reg))) = self.fds.get_mut(fd as usize) {
            let before = reg.datagrams;
            reg.datagrams = before.saturating_add(n as u32);
            if reg.armed && before < QUIET_AFTER && reg.datagrams >= QUIET_AFTER {
                // Leaves at the end of a turn's events, once the tasks
                // waiting on it have been woken (see `leave_busy_sets`).
                self.quieting.push((fd, reg.gen));
            }
        }
    }

    /// Takes the busy datagram sockets that no task waits on out of the
    /// poller's set; the others stay listed for the next turn.
    #[cold]
    #[inline(never)]
    fn leave_busy_sets(&mut self) {
        let poller = &self.poller;
        let mut quieting = mem::take(&mut self.quieting);
        quieting.retain(|&(fd, gen)| match self.fds.get_mut(fd as usize) {
            Some(Some(Slot::Pollable(reg))) if reg.gen == gen && reg.armed => {
                !leave_set(poller, reg, fd)
            }
            _ => false,
        });
        self.quieting = quieting;
    }

    pub(crate) fn drop_multi(&mut self, key: usize) {
        if let Some(EOp::Multi { .. }) = self.ops.get(key) {
            if let Some(ahead) = (!self.ahead.is_empty())
                .then(|| self.ahead.remove(&key))
                .flatten()
            {
                for (bid, _) in ahead.into_iter().flatten() {
                    self.pool.release(bid);
                }
            }
            self.ops.remove(key);
        }
    }

    // -------------------------------------------------------------------- park

    /// `true` if something can be collected without waiting.
    pub(crate) fn has_completions(&mut self) -> bool {
        if self.ready > 0 || self.completions.any() {
            return true;
        }
        if self.nothing_to_poll() {
            return false;
        }
        self.ready = self.wait_events(Some(Duration::ZERO));
        self.ready > 0
    }

    pub(crate) fn park(&mut self, mode: Park) -> Vec<Box<dyn Any>> {
        let n = if self.ready > 0 {
            mem::take(&mut self.ready)
        } else if matches!(mode, Park::Poll) && self.nothing_to_poll() {
            0
        } else {
            let timeout = match mode {
                _ if self.completions.any() => Some(Duration::ZERO),
                Park::Poll => Some(Duration::ZERO),
                Park::Wait(timeout) => timeout,
            };
            #[cfg(any(target_os = "macos", giong_inject_enobufs))]
            let timeout = if self.retries.is_empty() {
                timeout
            } else {
                self.retries.limit(timeout)
            };
            self.wait_events(timeout)
        };
        self.dispatch(n);
        #[cfg(any(target_os = "macos", giong_inject_enobufs))]
        if !self.retries.is_empty() {
            self.retries.wake_due();
        }
        if !self.quieting.is_empty() {
            self.leave_busy_sets();
        }
        self.collect_blocking();
        mem::take(&mut self.graveyard)
    }

    /// `true` if a look that cannot wait is to be skipped (see
    /// [`Poller::SKIP_EMPTY_POLL`]).
    #[inline]
    fn nothing_to_poll(&self) -> bool {
        P::SKIP_EMPTY_POLL && self.pollable == 0
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
                WAKE => self.wake.seen(),
                // Collected below; a ring left over from results that
                // were already taken is reset, or every wait would end
                // at once.
                DONE => self.completions.quiet(),
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
pub(super) fn set_nonblocking(fd: RawFd) -> io::Result<()> {
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
pub(super) fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: plain syscalls on a descriptor; the results are checked.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    type Platform = super::super::epoll::Epoll;
    #[cfg(all(not(target_os = "linux"), any(target_os = "macos", giong_libkqueue)))]
    type Platform = super::super::kqueue::Kqueue;

    /// A blocking job that finishes while its worker takes the results
    /// rings the bell after the results are gone. The next wait sees the
    /// ring and resets it: the one after waits again.
    #[test]
    #[cfg_attr(miri, ignore = "Miri cannot call epoll_wait or kevent")]
    fn a_late_ring_of_the_blocking_threads_does_not_end_every_wait() {
        let mut driver = Readiness::<Platform>::new(BufPool::new(2, 64).unwrap()).unwrap();
        driver.completions.push(usize::MAX, 0);
        let mut taken = Vec::new();
        driver.completions.take(&mut taken);
        assert_eq!(taken.len(), 1);
        // The ring of a job whose result was already taken.
        driver.completions.ring();
        driver.park(Park::Wait(Some(Duration::from_millis(1))));
        let wait = Duration::from_millis(50);
        let start = std::time::Instant::now();
        driver.park(Park::Wait(Some(wait)));
        assert!(
            start.elapsed() >= wait - Duration::from_millis(5),
            "{:?}",
            start.elapsed()
        );
    }
}
