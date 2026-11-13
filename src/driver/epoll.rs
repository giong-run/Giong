//! The epoll driver: for machines where io_uring is blocked or missing.
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
//!
//! An operation that finishes at once does not yield, so a task whose every
//! read and write succeeds immediately could keep the worker to itself. A
//! budget of [`BUDGET`] immediate operations per task poll prevents that:
//! once it is spent, operations report "pending" and wake their task again,
//! which puts it at the back of the run queue.

use std::any::Any;
use std::cell::Cell;
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::Duration;

use super::blocking::{self, Cancel, Completions};
use super::{Io, MultiIo, MultiKind, MultiPoll, Park, Start, BUFFER_SHIFT, F_BUFFER, F_MORE};
use crate::buf::BufPool;
use crate::slab::Slab;

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

/// epoll `data` of the worker's eventfd (cross-thread wake-ups).
const WAKE: u64 = u64::MAX;
/// epoll `data` of the eventfd the blocking threads poke.
const DONE: u64 = u64::MAX - 1;

/// Events collected per `epoll_wait`.
const MAX_EVENTS: usize = 1024;

/// Which readiness an operation waits for.
#[derive(Clone, Copy)]
enum Dir {
    Read,
    Write,
}

/// What the driver knows about a descriptor number.
enum Slot {
    /// Registered with epoll.
    Pollable(Registration),
    /// A regular file, a directory or a device: its reads and writes go to
    /// the blocking threads.
    Blocking,
}

/// One registered descriptor and the tasks waiting for it.
struct Registration {
    /// Matches the upper half of the epoll `data`, so that events for a
    /// descriptor number that has since been closed and reused are ignored.
    gen: u32,
    readers: Vec<Waker>,
    writers: Vec<Waker>,
}

enum Lifecycle {
    Waiting(Waker),
    Done(i32),
    /// The future was dropped; holds the resources until the thread is done.
    Ignored(Option<Box<dyn Any>>),
}

enum EOp {
    /// Waits for its descriptor; tried again when its future is polled.
    Ready { io: Io, connecting: bool },
    /// Runs on a blocking thread.
    Blocking {
        state: Lifecycle,
        cancel: Arc<Cancel>,
    },
    /// A multishot operation, armed once `io` is set.
    Multi { io: Option<MultiIo> },
}

/// What a non-blocking attempt came to.
enum Attempt {
    Done(i32),
    Wait(RawFd, Dir),
}

/// `-errno` of the last failed call.
fn neg_errno() -> i32 {
    -io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

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
            let res = retry(|| libc::pread64(fd, buf.cast(), len as usize, offset as i64));
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
            let res = retry(|| libc::pwrite64(fd, buf.cast(), len as usize, offset as i64));
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
        } => retry(|| libc::statx(dir, path, flags, mask, out) as isize),
        Io::Fsync { fd, datasync } => retry(|| {
            if datasync {
                libc::fdatasync(fd) as isize
            } else {
                libc::fsync(fd) as isize
            }
        }),
        Io::Fallocate { fd, offset, len } => {
            retry(|| libc::fallocate64(fd, 0, offset as i64, len as i64) as isize)
        }
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
        | Io::SendMsg { .. }
        | Io::RecvMsg { .. }
        | Io::ReadReady { .. } => unreachable!("socket operations never block a thread"),
    }
}

pub(crate) struct Epoll {
    epfd: OwnedFd,
    pool: Rc<BufPool>,
    /// The worker's eventfd, written by threads that wake it.
    wake_fd: RawFd,
    completions: Arc<Completions>,
    ops: Slab<EOp>,
    /// What is known about each descriptor, indexed by its number.
    fds: Vec<Option<Slot>>,
    next_gen: u32,
    events: Vec<libc::epoll_event>,
    /// Events collected by `has_completions` and not dispatched yet.
    ready: usize,
    done: Vec<(usize, i32)>,
    /// Blocking jobs whose result has not been collected yet.
    blocking: usize,
    graveyard: Vec<Box<dyn Any>>,
    /// `epoll_pwait2` (Linux 5.11) works here; else `epoll_wait`, whose
    /// timeout is in whole milliseconds.
    pwait2: bool,
}

impl Epoll {
    pub(crate) fn new(pool: Rc<BufPool>, wake_fd: RawFd) -> io::Result<Self> {
        // SAFETY: plain syscall; the result is checked.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        let epfd = unsafe { OwnedFd::from_raw_fd(fd) };
        let completions = Completions::new()?;
        let driver = Self {
            epfd,
            pool,
            wake_fd,
            completions,
            ops: Slab::new(),
            fds: Vec::new(),
            next_gen: 1,
            events: Vec::with_capacity(MAX_EVENTS),
            ready: 0,
            done: Vec::new(),
            blocking: 0,
            graveyard: Vec::new(),
            pwait2: true,
        };
        // The worker's eventfd is drained after every wake-up, so it must
        // not block when another thread got there first.
        set_nonblocking(wake_fd)?;
        driver.watch(wake_fd, WAKE)?;
        driver.watch(driver.completions.fd(), DONE)?;
        Ok(driver)
    }

    /// Adds an eventfd, level-triggered for reading.
    fn watch(&self, fd: RawFd, data: u64) -> io::Result<()> {
        let mut ev = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: data,
        };
        // SAFETY: valid descriptors and a valid event.
        if unsafe { libc::epoll_ctl(self.epfd.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn pool(&self) -> &Rc<BufPool> {
        &self.pool
    }

    // ------------------------------------------------------------ descriptors

    /// Registers `fd` with epoll (once) and makes it non-blocking.
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
        let mut ev = libc::epoll_event {
            events: (libc::EPOLLIN | libc::EPOLLOUT | libc::EPOLLRDHUP | libc::EPOLLET) as u32,
            u64: (u64::from(gen) << 32) | u64::from(fd as u32),
        };
        let epfd = self.epfd.as_raw_fd();
        // SAFETY: valid descriptors and a valid event.
        if unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, fd, &mut ev) } < 0 {
            let e = neg_errno();
            // Only possible if the number was registered before and the
            // registration outlived our record of it: take it over.
            // SAFETY: as above.
            if e != -libc::EEXIST
                || unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_MOD, fd, &mut ev) } < 0
            {
                return Err(if e == -libc::EEXIST { neg_errno() } else { e });
            }
        }
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
    /// blocking threads. Decided once per descriptor.
    fn pollable_file(&mut self, fd: RawFd) -> bool {
        let Ok(i) = usize::try_from(fd) else {
            return false;
        };
        match self.fds.get(i) {
            Some(Some(Slot::Pollable(_))) => return true,
            Some(Some(Slot::Blocking)) => return false,
            _ => {}
        }
        // SAFETY: all-zero is a valid `stat`, which fstat fills in.
        let mut st: libc::stat64 = unsafe { mem::zeroed() };
        // SAFETY: a valid pointer to a `stat64`.
        let streams = unsafe { libc::fstat64(fd, &mut st) } == 0
            && matches!(st.st_mode & libc::S_IFMT, libc::S_IFIFO | libc::S_IFSOCK);
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

    /// Closes a descriptor the caller gives up.
    pub(crate) fn close_fd(&mut self, fd: RawFd) {
        self.forget(fd);
        // SAFETY: the caller owns `fd` and gives it up here. Closing also
        // removes it from the epoll set (no other descriptor shares it).
        unsafe { libc::close(fd) };
    }

    // ---------------------------------------------------------------- attempts

    /// Tries a socket operation without blocking.
    fn attempt(&mut self, io: &Io, connecting: &mut bool) -> Attempt {
        let fd = match *io {
            Io::Connect { fd, .. }
            | Io::Recv { fd, .. }
            | Io::Send { fd, .. }
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
        // SAFETY (all calls below): the pointers come from `Op` resources,
        // valid while the operation exists; the descriptor is non-blocking.
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
                Io::Recv { fd, buf, len } => (
                    retry(|| libc::recv(fd, buf.cast(), len as usize, libc::MSG_DONTWAIT)),
                    Dir::Read,
                ),
                Io::Send {
                    fd,
                    buf,
                    len,
                    flags,
                } => (
                    retry(|| libc::send(fd, buf.cast(), len as usize, flags | libc::MSG_DONTWAIT)),
                    Dir::Write,
                ),
                Io::SendMsg { fd, msg, flags } => (
                    retry(|| libc::sendmsg(fd, msg, flags as i32 | libc::MSG_DONTWAIT)),
                    Dir::Write,
                ),
                Io::RecvMsg { fd, msg } => {
                    (retry(|| libc::recvmsg(fd, msg, libc::MSG_DONTWAIT)), Dir::Read)
                }
                // A pipe or a socket: offsets do not apply.
                Io::ReadReady { fd, buf, len } | Io::Read { fd, buf, len, .. } => {
                    (retry(|| libc::read(fd, buf.cast(), len as usize)), Dir::Read)
                }
                Io::Write { fd, buf, len, .. } => {
                    (retry(|| libc::write(fd, buf.cast(), len as usize)), Dir::Write)
                }
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
            | Io::SendMsg { .. }
            | Io::RecvMsg { .. }
            | Io::ReadReady { .. } => false,
        }
    }

    fn start_blocking(&mut self, io: &Io, waker: &Waker) -> usize {
        let cancel = Cancel::new();
        let key = self.ops.insert(EOp::Blocking {
            state: Lifecycle::Waiting(waker.clone()),
            cancel: cancel.clone(),
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

    pub(crate) fn cancel_op(&mut self, key: usize, data: Option<Box<dyn Any>>) {
        match self.ops.get_mut(key) {
            Some(EOp::Blocking {
                state: state @ Lifecycle::Waiting(_),
                cancel,
            }) => {
                // The thread may be using the resources: keep them until it
                // reports. A job that has not started yet skips its call.
                cancel.cancel();
                *state = Lifecycle::Ignored(data);
            }
            Some(EOp::Ready { .. })
            | Some(EOp::Blocking {
                state: Lifecycle::Done(_),
                ..
            }) => {
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
            return MultiPoll::Item(e, 0);
        }
        let outcome = match io {
            MultiIo::Accept { fd } => self.accept(fd),
            MultiIo::Recv { fd } => self.recv_into_pool(fd),
            MultiIo::RecvMsg { fd, hdr } => self.recvmsg_into_pool(fd, hdr),
        };
        match outcome {
            Some(Ok((res, flags))) => MultiPoll::Item(res, flags),
            Some(Err(res)) if is_would_block(res) => {
                self.wait(fd, Dir::Read, waker);
                MultiPoll::Pending
            }
            Some(Err(res)) => {
                // Like a multishot that ended with an error: the next poll
                // arms it again.
                self.disarm(key);
                MultiPoll::Item(res, 0)
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
        let flags = libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK;
        // SAFETY: null address pointers ask for no peer address.
        let res = retry(|| unsafe {
            libc::accept4(fd, std::ptr::null_mut(), std::ptr::null_mut(), flags) as isize
        });
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
        let (name_space, control_space) = unsafe {
            (
                (*hdr).msg_namelen as usize,
                (*hdr).msg_controllen as usize,
            )
        };
        let payload_at = OUT_HEADER + name_space + control_space;
        if len < payload_at {
            return Some(Err(-libc::ENOBUFS));
        }
        // SAFETY (this block): every pointer stays inside the free pool
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

    /// `epoll_wait` into `events`; `None` waits without a time limit.
    fn wait_events(&mut self, timeout: Option<Duration>) -> usize {
        self.events.clear();
        let epfd = self.epfd.as_raw_fd();
        let buf = self.events.as_mut_ptr();
        let n = loop {
            let res = if self.pwait2 {
                let ts = timeout.map(|d| libc::timespec {
                    tv_sec: d.as_secs().min(i64::MAX as u64) as libc::time_t,
                    tv_nsec: d.subsec_nanos() as libc::c_long,
                });
                let ts_ptr = ts
                    .as_ref()
                    .map_or(std::ptr::null(), |t| t as *const libc::timespec);
                // SAFETY: `buf` has room for MAX_EVENTS events; `ts_ptr` is
                // null or points at a live timespec.
                unsafe {
                    libc::syscall(
                        libc::SYS_epoll_pwait2,
                        epfd,
                        buf,
                        MAX_EVENTS as libc::c_int,
                        ts_ptr,
                        std::ptr::null::<libc::sigset_t>(),
                        0usize,
                    )
                }
            } else {
                // Whole milliseconds, rounded up so a timer is never early.
                let ms = timeout.map_or(-1, |d| {
                    let ms = d.as_nanos().div_ceil(1_000_000);
                    ms.min(i32::MAX as u128) as libc::c_int
                });
                // SAFETY: as above.
                unsafe { libc::epoll_wait(epfd, buf, MAX_EVENTS as libc::c_int, ms) as libc::c_long }
            };
            if res >= 0 {
                break res as usize;
            }
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::EINTR) => break 0,
                // A kernel older than 5.11, or a seccomp profile that does
                // not know the call: use epoll_wait from now on.
                Some(libc::ENOSYS | libc::EPERM) if self.pwait2 => self.pwait2 = false,
                _ => panic!(
                    "giong: epoll_wait failed: {}",
                    io::Error::last_os_error()
                ),
            }
        };
        // SAFETY: the kernel initialised `n <= MAX_EVENTS` events.
        unsafe { self.events.set_len(n) };
        n
    }

    /// Wakes the tasks waiting for the descriptors in the first `n` events.
    fn dispatch(&mut self, n: usize) {
        const READABLE: u32 = (libc::EPOLLIN
            | libc::EPOLLPRI
            | libc::EPOLLRDHUP
            | libc::EPOLLHUP
            | libc::EPOLLERR) as u32;
        const WRITABLE: u32 = (libc::EPOLLOUT | libc::EPOLLHUP | libc::EPOLLERR) as u32;
        for i in 0..n {
            // Copied out: the struct is packed on x86-64.
            let (events, data) = (self.events[i].events, self.events[i].u64);
            match data {
                WAKE => {
                    let mut count: u64 = 0;
                    // SAFETY: reads 8 bytes into a valid u64 from the
                    // non-blocking eventfd.
                    unsafe {
                        libc::read(
                            self.wake_fd,
                            &mut count as *mut u64 as *mut libc::c_void,
                            8,
                        )
                    };
                }
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
                    if events & READABLE != 0 {
                        for waker in reg.readers.drain(..) {
                            waker.wake();
                        }
                    }
                    if events & WRITABLE != 0 {
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
            let remove = match self.ops.get_mut(key) {
                Some(EOp::Blocking { state, .. }) => match mem::replace(state, Lifecycle::Done(res)) {
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
                    Lifecycle::Done(_) => false,
                },
                _ => false,
            };
            if remove {
                self.ops.remove(key);
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
    if libc::getpeername(fd, (&mut addr as *mut libc::sockaddr_storage).cast(), &mut len) < 0 {
        return neg_errno();
    }
    0
}

/// Completion flags for data in pool buffer `bid`.
fn pooled_flags(bid: u16) -> u32 {
    F_BUFFER | (u32::from(bid) << BUFFER_SHIFT) | F_MORE
}

/// Puts `fd` in non-blocking mode.
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let mut on: libc::c_int = 1;
    // SAFETY: FIONBIO reads an int from a valid pointer.
    if unsafe { libc::ioctl(fd, libc::FIONBIO, &mut on) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
