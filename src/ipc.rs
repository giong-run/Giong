//! Messages between processes on the same machine, through shared memory.
//!
//! A channel is a pair of byte rings in memory both processes map, one per
//! direction, set up over a Unix socket. Sending a message copies it into
//! the ring; receiving hands out the bytes where they lie. While both sides
//! are busy no system call is made at all; a side that runs out of messages
//! (or of room) sleeps on an eventfd and is woken only when the other side
//! sees it sleeping. Compared with a Unix socket, this removes the kernel
//! from the data path: see `bench/results/ipc-giong/README.md` for
//! measurements.
//!
//! Linux only: the channel is built on `memfd` and eventfds, so the module
//! is not compiled on macOS.
//!
//! ```no_run
//! # giong::block_on(async {
//! // Process A
//! let listener = giong::ipc::IpcListener::bind("/run/app/ipc.sock", 1 << 20)?;
//! let (mut tx, mut rx) = listener.accept().await?;
//! while let Some(msg) = rx.recv().await? {
//!     tx.send(&msg).await?; // echo
//! }
//! # Ok::<_, std::io::Error>(())
//! # }).unwrap();
//! ```
//!
//! ```no_run
//! # giong::block_on(async {
//! // Process B
//! let (mut tx, mut rx) = giong::ipc::connect("/run/app/ipc.sock", 1 << 20).await?;
//! tx.send(b"ping").await?;
//! let reply = rx.recv().await?.expect("open");
//! assert_eq!(&*reply, b"ping");
//! # Ok::<_, std::io::Error>(())
//! # }).unwrap();
//! ```
//!
//! **Bounded.** The connecting side chooses the capacity of each direction
//! (a power of two from 4 KiB to 1 GiB); the listener refuses more than its
//! limit. A message is at most a quarter of the capacity
//! ([`Sender::max_message`]). A full ring makes [`Sender::send`] wait: memory
//! never grows.
//!
//! **Trust.** Who may connect is decided by the socket file's permissions.
//! The peer process can write into the shared memory at any time; giong
//! checks every position and length it reads from there, so a broken or
//! hostile peer makes the channel fail with `GIONG-E0091`, and cannot make
//! this process read or write outside the mapping. It can change the bytes
//! of a message it already sent while you read them, so treat message
//! contents like data read from a socket.
//!
//! **Ending.** Dropping a [`Sender`] ends the stream of messages the other
//! side receives ([`Receiver::recv`] returns `Ok(None)` once it has read the
//! rest). A peer process that exits without dropping its halves is noticed
//! through the Unix socket: `GIONG-E0094`.

mod ring;

use std::cell::{Cell, RefCell};
use std::future::{poll_fn, Future};
use std::io;
use std::ops::Deref;
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::Poll;
use std::time::Duration;

use crate::driver::Io;
use crate::error::{raise, Code};
use crate::executor::{JoinError, JoinHandle};
use crate::kbox::KernelBox;
use crate::net::{UnixListener, UnixStream};
use crate::op::Op;
use crate::runtime;
use ring::{Corrupt, Dir, Push, Ring, ASLEEP, AWAKE, CONSUMER_CLOSED, PRODUCER_CLOSED};

/// The eventfds of a channel: who waits on each.
const D0_DATA: usize = 0; // acceptor's receiver
const D0_SPACE: usize = 1; // connector's sender
const D1_DATA: usize = 2; // connector's receiver
const D1_SPACE: usize = 3; // acceptor's sender
const HANDSHAKE_LEN: usize = 16;
/// How many times a receiver that ran out of records looks at the ring
/// before it waits: about a microsecond.
const BRIEF_SPINS: u32 = 64;
const FDS: usize = 5;

/// Handshake answers, one byte from the listener.
const ACCEPTED: u8 = 0;
const REFUSED_CAPACITY: u8 = 1;
const REFUSED_INVALID: u8 = 2;

/// A descriptor closed through the worker's driver, like giong's sockets.
struct Fd(RawFd);

impl Drop for Fd {
    fn drop(&mut self) {
        let fd = self.0;
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

fn last_os_error<T>(rc: T) -> io::Result<T>
where
    T: PartialOrd + Default,
{
    if rc < T::default() {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// What both halves of one channel share: the mapping, the eventfds and the
/// socket that tells when the peer process is gone.
struct Shared {
    base: *mut u8,
    len: usize,
    capacity: usize,
    efds: [Fd; 4],
    control: UnixStream,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: mapped in `map` with this length; no `Ring` outlives the
        // halves that hold this `Rc`.
        unsafe { libc::munmap(self.base.cast(), self.len) };
    }
}

fn check_capacity(capacity: usize, what: &str) -> io::Result<()> {
    if !capacity.is_power_of_two() || !(ring::MIN_CAPACITY..=ring::MAX_CAPACITY).contains(&capacity)
    {
        return Err(raise(
            Code::IpcCapacity,
            format!("{what} {capacity} is not a power of two from 4096 to 2^30 bytes"),
            "pass a power of two, such as 1 << 20 for 1 MiB per direction.",
        ));
    }
    Ok(())
}

fn corrupt(c: Corrupt) -> io::Error {
    raise(
        Code::IpcCorrupted,
        format!("the shared memory of an IPC channel is corrupted: {}", c.0),
        "the other process wrote into the channel's memory outside the rules; check that it \
         uses the same giong version and does not write to the mapping itself, then reconnect.",
    )
}

fn peer_gone() -> io::Error {
    raise(
        Code::IpcPeerGone,
        "the other process of an IPC channel went away without closing it",
        "the other process exited or crashed; reconnect when it is back. Messages it had sent \
         before were delivered.",
    )
}

fn handshake_failed(why: &str) -> io::Error {
    raise(
        Code::IpcHandshake,
        format!("IPC handshake failed: {why}"),
        "connect giong::ipc::connect to a giong::ipc::IpcListener of the same giong version, \
         not to another kind of Unix socket.",
    )
}

fn map(fd: RawFd, len: usize) -> io::Result<*mut u8> {
    // SAFETY: a fresh shared mapping of a sealed memfd at least `len` long
    // (the size is checked, and the seals stop either side shrinking it).
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(p.cast())
}

fn eventfd() -> io::Result<Fd> {
    // SAFETY: plain system call.
    let fd = last_os_error(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
    Ok(Fd(fd))
}

fn dup(fd: RawFd) -> io::Result<Fd> {
    // SAFETY: plain system call on a descriptor we hold.
    let fd = last_os_error(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })?;
    Ok(Fd(fd))
}

/// Pokes an eventfd. A failure (the counter is full, which takes 2^64 - 1
/// pokes nobody read) loses nothing: the waiter is woken anyway.
fn wake(fd: &Fd) {
    let one = 1u64;
    // SAFETY: writes 8 bytes from a live u64.
    unsafe { libc::write(fd.0, (&one as *const u64).cast(), 8) };
}

/// Wakes the other side if it said it sleeps. The flag is read first, so
/// that the common case (it does not sleep) writes nothing to its cache
/// line. Sequentially consistent: see `Ring::push`.
fn wake_if_sleeping(flag: &AtomicU32, efd: &Fd) {
    if flag.load(Ordering::SeqCst) == ASLEEP
        && flag
            .compare_exchange(ASLEEP, AWAKE, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    {
        wake(efd);
    }
}

/// Watches one direction for its consumer while the consumer's worker
/// spins: new records show as a moved `tail`, with no system call on
/// either side.
struct RingProbe {
    dir: *const Dir,
    /// The consumer's position when it went to wait.
    pos: u64,
    /// Set by [`arm`](runtime::SpinProbe::arm), read by the waiting
    /// consumer.
    armed: Rc<Cell<bool>>,
    /// Calls of `ready`, to read the producer's line on one in
    /// [`LOOK_EVERY`] only.
    looks: Cell<u32>,
    /// Keeps the mapping `dir` points into alive.
    _shared: Rc<Shared>,
}

/// The spin loop calls `ready` every few tens of nanoseconds. Reading the
/// line the producer writes that often takes it from the producer before
/// nearly every message it sends; one look in four lets it write several
/// messages in a row, at a cost of about a hundred nanoseconds of latency.
const LOOK_EVERY: u32 = 4;

impl RingProbe {
    fn dir(&self) -> &Dir {
        // SAFETY: inside the mapping `_shared` keeps alive; all atomics.
        unsafe { &*self.dir }
    }
}

impl runtime::SpinProbe for RingProbe {
    fn ready(&self) -> bool {
        let n = self.looks.get().wrapping_add(1);
        self.looks.set(n);
        if n % LOOK_EVERY != 0 {
            return false;
        }
        // A hint: the woken consumer reads the ring properly.
        let dir = self.dir();
        dir.tail.0.load(Ordering::Relaxed) != self.pos
            || dir.flags.0.load(Ordering::Relaxed) & PRODUCER_CLOSED != 0
    }
    fn arm(&self) {
        // From now on the producer wakes the consumer through the eventfd.
        // The woken consumer looks at the ring again (after its own
        // sequentially consistent store of ASLEEP) before it sleeps. The
        // probe is only registered while its consumer waits on this
        // worker's thread, so nothing races with this store here.
        self.dir()
            .consumer_sleeping
            .0
            .store(ASLEEP, Ordering::SeqCst);
        self.armed.set(true);
    }
}

/// Takes a probe that has not fired off the worker when the consumer stops
/// waiting for it: woken for another reason, or its `recv` was cancelled
/// after the worker armed it (then the producer is told the consumer is
/// awake again).
struct ProbeGuard<'a> {
    id: Option<u64>,
    flag: &'a AtomicU32,
    armed: &'a Cell<bool>,
    finished: bool,
}

impl Drop for ProbeGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            runtime::try_with_worker(|w| w.remove_probe(id));
        }
        if !self.finished && self.armed.get() {
            self.flag.store(AWAKE, Ordering::SeqCst);
        }
    }
}

/// Watches the control socket for the peer going away: a read that only
/// completes when the peer process closes its end.
struct Watch {
    fd: Fd,
    op: Option<Op<Vec<u8>>>,
    gone: bool,
}

impl Watch {
    fn new(control: &UnixStream) -> io::Result<Self> {
        Ok(Self {
            fd: dup(control.as_raw_fd())?,
            op: None,
            gone: false,
        })
    }

    /// Ready when the peer is gone (or broke the protocol by writing).
    fn poll(&mut self, cx: &mut std::task::Context<'_>) -> Poll<()> {
        if self.gone {
            return Poll::Ready(());
        }
        loop {
            let op = self.op.get_or_insert_with(|| {
                let mut buf = Vec::with_capacity(1);
                let io = Io::ReadReady {
                    fd: self.fd.0,
                    buf: buf.as_mut_ptr(),
                    len: 1,
                };
                // SAFETY: `buf` points at the `Vec`'s heap buffer, which
                // stays put when the `Vec` moves.
                unsafe { Op::new(io, buf) }
            });
            match Pin::new(op).poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready((res, _, _)) => {
                    self.op = None;
                    if res == -libc::EINTR || res == -libc::EAGAIN {
                        continue;
                    }
                    self.gone = true;
                    return Poll::Ready(());
                }
            }
        }
    }
}

/// Sleeps until `efd` is poked; `Err` when the peer process went away.
async fn sleep(efd: RawFd, watch: &mut Watch) -> io::Result<()> {
    let mut buf: Vec<u8> = Vec::with_capacity(8);
    let io = Io::ReadReady {
        fd: efd,
        buf: buf.as_mut_ptr(),
        len: 8,
    };
    // SAFETY: as in `Watch::poll`.
    let mut op = unsafe { Op::new(io, buf) };
    poll_fn(|cx| {
        if Pin::new(&mut op).poll(cx).is_ready() {
            return Poll::Ready(Ok(()));
        }
        if watch.poll(cx).is_ready() {
            return Poll::Ready(Err(peer_gone()));
        }
        Poll::Pending
    })
    .await
}

/// The sending half of a channel.
///
/// Not `Send`: it belongs to the worker it was created on, like giong's
/// sockets.
pub struct Sender {
    ring: Ring,
    wait: usize,
    watch: Watch,
    /// What has been written and not yet published.
    pending: Rc<Pending>,
    /// Unpublished bytes after which `send` publishes at once.
    publish_at: u64,
    shared: Rc<Shared>,
}

/// The producer's `tail`, published once per turn of the worker (see
/// [`runtime::Publish`]) or when much is waiting, rather than after every
/// message: the consumer polls that line, and a store to it per message
/// takes it back from the consumer's core each time.
struct Pending {
    dir: *const Dir,
    /// The position up to which records are written.
    tail: Cell<u64>,
    /// The position last stored to `tail`.
    published: Cell<u64>,
    /// In the worker's list for the end of the turn.
    queued: Cell<bool>,
    wake: usize,
    /// Keeps the mapping `dir` points into, and the eventfds, alive.
    shared: Rc<Shared>,
}

impl Pending {
    fn dir(&self) -> &Dir {
        // SAFETY: inside the mapping `shared` keeps alive; all atomics.
        unsafe { &*self.dir }
    }

    /// Makes every written record visible, and wakes a sleeping consumer.
    fn flush(&self) {
        let tail = self.tail.get();
        if tail == self.published.get() {
            return;
        }
        self.published.set(tail);
        // Sequentially consistent, as is the look at `consumer_sleeping`
        // that follows: with the consumer's store of that flag and its next
        // look at `tail` (also sequentially consistent), one of the two
        // sees the other, so a wake-up is never missed.
        self.dir().tail.0.store(tail, Ordering::SeqCst);
        wake_if_sleeping(
            &self.dir().consumer_sleeping.0,
            &self.shared.efds[self.wake],
        );
    }
}

impl runtime::Publish for Pending {
    fn publish(&self) {
        self.queued.set(false);
        self.flush();
    }
}

impl Sender {
    /// The largest message [`send`](Self::send) takes: a quarter of the
    /// channel's capacity, less 8 bytes.
    pub fn max_message(&self) -> usize {
        ring::max_message(self.shared.capacity)
    }

    /// Copies `msg` into the channel. Waits while the channel is full.
    ///
    /// The receiver sees the message once this task gives the worker back
    /// (it awaits something not ready, or returns), when this task waits in
    /// [`Receiver::recv`], when a sixteenth of the capacity is waiting, or
    /// when the `Sender` is dropped: a burst of messages is published with
    /// one store to the memory the receiver polls. A task that sends and
    /// then computes for long without awaiting holds the message back
    /// until then.
    ///
    /// Fails with `GIONG-E0090` for a message longer than
    /// [`max_message`](Self::max_message), `GIONG-E0095` once the other
    /// side dropped its [`Receiver`], and `GIONG-E0094` when the other
    /// process is gone. That is noticed while waiting for room: with room
    /// in the channel, a message to a process that has just died is
    /// written and `Ok` returned, as with a socket.
    ///
    /// Cancel safe: if the future is dropped, the message was either sent
    /// whole or not at all.
    pub async fn send(&mut self, msg: &[u8]) -> io::Result<()> {
        if msg.len() > self.max_message() {
            return Err(raise(
                Code::IpcMessageTooLarge,
                format!(
                    "an IPC message of {} bytes is longer than this channel's limit of {} bytes",
                    msg.len(),
                    self.max_message()
                ),
                "connect with a larger capacity (a message may take a quarter of it), or split \
                 the message.",
            ));
        }
        loop {
            if self.ring.dir().flags.0.load(Ordering::Acquire) & CONSUMER_CLOSED != 0 {
                return Err(raise(
                    Code::IpcReceiverClosed,
                    "the other side of the IPC channel dropped its receiver",
                    "stop sending on this channel; the other process no longer reads it.",
                ));
            }
            if self.watch.gone {
                return Err(peer_gone());
            }
            if self.ring.push(msg).map_err(corrupt)? == Push::Done {
                let pending = &self.pending;
                pending.tail.set(self.ring.pos());
                if self.ring.pos() - pending.published.get() >= self.publish_at {
                    pending.flush();
                } else if !pending.queued.replace(true) {
                    let p: Rc<dyn runtime::Publish> = pending.clone();
                    runtime::with_worker(|w| w.defer_publish(p));
                }
                return Ok(());
            }
            // Full: the consumer must see everything before it can make room.
            self.pending.flush();
            self.ring
                .dir()
                .producer_sleeping
                .0
                .store(1, Ordering::SeqCst);
            if self.ring.has_room(msg.len()).map_err(corrupt)?
                || self.ring.dir().flags.0.load(Ordering::Acquire) & CONSUMER_CLOSED != 0
            {
                self.ring
                    .dir()
                    .producer_sleeping
                    .0
                    .store(0, Ordering::Relaxed);
                continue;
            }
            let efd = self.shared.efds[self.wait].0;
            let slept = sleep(efd, &mut self.watch).await;
            self.ring
                .dir()
                .producer_sleeping
                .0
                .store(0, Ordering::Relaxed);
            slept?;
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.pending.flush();
        self.ring
            .dir()
            .flags
            .0
            .fetch_or(PRODUCER_CLOSED, Ordering::AcqRel);
        wake(&self.shared.efds[self.pending.wake]);
    }
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("capacity", &self.shared.capacity)
            .finish()
    }
}

/// The receiving half of a channel.
///
/// Not `Send`: it belongs to the worker it was created on.
pub struct Receiver {
    ring: Ring,
    wait: usize,
    wake: usize,
    watch: Watch,
    shared: Rc<Shared>,
}

impl Receiver {
    /// The next message, or `None` once the other side dropped its
    /// [`Sender`] and every message it sent was received.
    ///
    /// The message is read in place, in the shared memory; dropping it
    /// gives its room back to the sender. Fails with `GIONG-E0094` when the
    /// other process went away without dropping its sender (after the
    /// messages it sent before), and `GIONG-E0091` when it corrupted the
    /// channel.
    ///
    /// Cancel safe: if the future is dropped, no message is lost.
    pub async fn recv(&mut self) -> io::Result<Option<Message<'_>>> {
        loop {
            if let Some((ptr, len, total)) = self.ring.peek().map_err(corrupt)? {
                return Ok(Some(Message {
                    rx: self,
                    ptr,
                    len,
                    total,
                }));
            }
            let closed = self.ring.dir().flags.0.load(Ordering::Acquire) & PRODUCER_CLOSED != 0;
            if closed || self.watch.gone {
                if self.ring.has_data() {
                    continue;
                }
                return if closed { Ok(None) } else { Err(peer_gone()) };
            }
            // Messages this worker sent and still holds back go out first:
            // the answer awaited here may depend on them.
            runtime::with_worker(|w| w.publish_now());
            // A record is often a few hundred nanoseconds away when the
            // producer is streaming: look for it briefly before paying for
            // a wait (a probe, or a sleep in the kernel and a wake-up).
            if self.ring_moves_soon() {
                continue;
            }
            if runtime::with_worker(|w| w.spins()) && self.spin_wait().await {
                continue;
            }
            self.ring
                .dir()
                .consumer_sleeping
                .0
                .store(ASLEEP, Ordering::SeqCst);
            if self.ring.has_data()
                || self.ring.dir().flags.0.load(Ordering::Acquire) & PRODUCER_CLOSED != 0
            {
                self.ring
                    .dir()
                    .consumer_sleeping
                    .0
                    .store(0, Ordering::Relaxed);
                continue;
            }
            let efd = self.shared.efds[self.wait].0;
            // A gone peer is reported once what it sent has been read.
            let _ = sleep(efd, &mut self.watch).await;
            self.ring
                .dir()
                .consumer_sleeping
                .0
                .store(0, Ordering::Relaxed);
        }
    }

    /// Polls the ring for up to about a microsecond.
    fn ring_moves_soon(&self) -> bool {
        for _ in 0..BRIEF_SPINS {
            if self.ring.tail_moved() {
                return true;
            }
            // Fewer reads of the line the producer writes, so that it is
            // not taken from the producer on every message it sends.
            for _ in 0..4 {
                std::hint::spin_loop();
            }
        }
        false
    }

    /// Waits for the next record while the worker spins, watching the ring
    /// from the worker's spin loop. `true` when the consumer was woken
    /// because something arrived (look again); `false` when the worker
    /// stopped spinning and the producer now wakes through the eventfd
    /// (the flag is [`ASLEEP`]): wait in the kernel.
    async fn spin_wait(&mut self) -> bool {
        if self.ring.has_data()
            || self.ring.dir().flags.0.load(Ordering::Acquire) & PRODUCER_CLOSED != 0
        {
            return true;
        }
        // The shared flag stays AWAKE while the worker spins, so the
        // producer, which reads it after every message, keeps its copy of
        // the line: the probe is what notices the next message.
        let armed = Rc::new(Cell::new(false));
        let mut probe = Some(RingProbe {
            dir: self.ring.dir_ptr(),
            pos: self.ring.pos(),
            armed: armed.clone(),
            looks: Cell::new(0),
            _shared: self.shared.clone(),
        });
        let mut guard = ProbeGuard {
            id: None,
            flag: &self.ring.dir().consumer_sleeping.0,
            armed: &armed,
            finished: false,
        };
        // Registers the probe with this task's waker, then waits for it.
        poll_fn(|cx| match probe.take() {
            Some(p) => {
                let id = runtime::with_worker(|w| w.add_probe(Box::new(p), cx.waker().clone()));
                guard.id = Some(id);
                Poll::Pending
            }
            None => Poll::Ready(()),
        })
        .await;
        guard.finished = true;
        drop(guard);
        // Not armed: the probe saw work (or the task was woken for another
        // reason); look again. Armed: the worker is about to block and the
        // flag says ASLEEP: wait in the kernel.
        !armed.get()
    }

    fn release(&mut self, total: u64) {
        self.ring.release(total);
        wake_if_sleeping(
            &self.ring.dir().producer_sleeping.0,
            &self.shared.efds[self.wake],
        );
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.ring
            .dir()
            .flags
            .0
            .fetch_or(CONSUMER_CLOSED, Ordering::AcqRel);
        wake(&self.shared.efds[self.wake]);
    }
}

impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("capacity", &self.shared.capacity)
            .finish()
    }
}

/// A received message, in the channel's shared memory. Dropping it gives
/// its room back to the sender.
pub struct Message<'a> {
    rx: &'a mut Receiver,
    ptr: *const u8,
    len: usize,
    total: u64,
}

impl Deref for Message<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        // SAFETY: `ring.peek` checked that the bytes lie inside the mapping,
        // which the receiver keeps alive; the producer does not reuse them
        // until `release`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for Message<'_> {
    fn drop(&mut self) {
        self.rx.release(self.total);
    }
}

impl std::fmt::Debug for Message<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Message").field("len", &self.len).finish()
    }
}

fn halves(shared: Shared, connector: bool) -> io::Result<(Sender, Receiver)> {
    let (tx_dir, rx_dir) = if connector { (0, 1) } else { (1, 0) };
    let (tx_wait, tx_wake, rx_wait, rx_wake) = if connector {
        (D0_SPACE, D0_DATA, D1_DATA, D1_SPACE)
    } else {
        (D1_SPACE, D1_DATA, D0_DATA, D0_SPACE)
    };
    let tx_watch = Watch::new(&shared.control)?;
    let rx_watch = Watch::new(&shared.control)?;
    let shared = Rc::new(shared);
    // SAFETY: the mapping has the checked capacity and layout and lives in
    // `shared`, which each half holds; this process makes one producer and
    // one consumer per direction.
    let (tx_ring, rx_ring) = unsafe {
        (
            Ring::new(shared.base, shared.capacity, tx_dir),
            Ring::new(shared.base, shared.capacity, rx_dir),
        )
    };
    Ok((
        Sender {
            pending: Rc::new(Pending {
                dir: tx_ring.dir_ptr(),
                tail: Cell::new(tx_ring.pos()),
                published: Cell::new(tx_ring.pos()),
                queued: Cell::new(false),
                wake: tx_wake,
                shared: shared.clone(),
            }),
            publish_at: (shared.capacity / 16) as u64,
            ring: tx_ring,
            wait: tx_wait,
            watch: tx_watch,
            shared: shared.clone(),
        },
        Receiver {
            ring: rx_ring,
            wait: rx_wait,
            wake: rx_wake,
            watch: rx_watch,
            shared,
        },
    ))
}

/// A `msghdr` with room for the handshake and its descriptors, kept where
/// the kernel can write it.
#[repr(C)]
struct HandshakeMsg {
    hdr: libc::msghdr,
    iov: libc::iovec,
    data: [u8; HANDSHAKE_LEN],
    /// `CMSG_SPACE(5 * 4)` is 40 bytes on Linux; 64 leaves room.
    cmsg: [u64; 8],
}

fn handshake_msg() -> KernelBox<HandshakeMsg> {
    // SAFETY: all-zero bytes are a valid `HandshakeMsg` (plain C data).
    let msg: KernelBox<HandshakeMsg> = KernelBox::new(unsafe { std::mem::zeroed() });
    let p = msg.as_ptr();
    // SAFETY: `p` is a live, unshared allocation; the pointers stored point
    // into it, and it never moves.
    unsafe {
        (*p).iov.iov_base = ptr::addr_of_mut!((*p).data).cast();
        (*p).iov.iov_len = HANDSHAKE_LEN;
        (*p).hdr.msg_iov = ptr::addr_of_mut!((*p).iov);
        (*p).hdr.msg_iovlen = 1;
        (*p).hdr.msg_control = ptr::addr_of_mut!((*p).cmsg).cast();
        (*p).hdr.msg_controllen = std::mem::size_of::<[u64; 8]>() as _;
    }
    msg
}

async fn answer(control: &UnixStream, status: u8) -> io::Result<()> {
    control.write_all(vec![status]).await.0
}

/// Connects to the [`IpcListener`] at `path` and sets up a channel of
/// `capacity` bytes in each direction (a power of two from 4 KiB to 1 GiB;
/// both processes map `2 * capacity` bytes plus a page).
///
/// Fails with `GIONG-E0092` for a capacity out of range or above the
/// listener's limit, `GIONG-E0093` when the socket is not a giong IPC
/// listener, and with the kernel's error when nothing listens at `path`.
///
/// Cancel safe: if the future is dropped, nothing is left behind.
pub async fn connect<P: AsRef<Path>>(path: P, capacity: usize) -> io::Result<(Sender, Receiver)> {
    check_capacity(capacity, "IPC capacity")?;
    let control = UnixStream::connect(path).await?;
    let len = ring::mapping_len(capacity);
    // SAFETY: plain system calls; the name is a valid C string.
    let mem = Fd(last_os_error(unsafe {
        libc::memfd_create(
            c"giong-ipc".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    })?);
    // SAFETY: as above.
    unsafe {
        last_os_error(libc::ftruncate(mem.0, len as libc::off_t))?;
        last_os_error(libc::fcntl(
            mem.0,
            libc::F_ADD_SEALS,
            libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL,
        ))?;
    }
    let base = map(mem.0, len)?;
    let efds = [eventfd()?, eventfd()?, eventfd()?, eventfd()?];
    let shared = Shared {
        base,
        len,
        capacity,
        efds,
        control,
    };
    // SAFETY: the mapping is at least a page long and page-aligned.
    let header = unsafe { &(*base.cast::<ring::Control>()).header };
    header.magic.store(ring::MAGIC, Ordering::Relaxed);
    header.version.store(ring::VERSION, Ordering::Relaxed);
    header.capacity.store(capacity as u32, Ordering::Release);

    let msg = handshake_msg();
    let p = msg.as_ptr();
    // SAFETY: `p` is live; nothing else uses it yet. The control buffer is
    // 64 bytes, larger than `CMSG_SPACE(20)`.
    unsafe {
        let mut data = [0u8; HANDSHAKE_LEN];
        data[..8].copy_from_slice(&ring::MAGIC.to_le_bytes());
        data[8..12].copy_from_slice(&ring::VERSION.to_le_bytes());
        data[12..16].copy_from_slice(&(capacity as u32).to_le_bytes());
        ptr::addr_of_mut!((*p).data).write(data);
        let fds_len = (FDS * std::mem::size_of::<RawFd>()) as u32;
        (*p).hdr.msg_controllen = libc::CMSG_SPACE(fds_len) as _;
        let cmsg = libc::CMSG_FIRSTHDR(ptr::addr_of!((*p).hdr));
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fds_len) as _;
        let fds = [
            mem.0,
            shared.efds[0].0,
            shared.efds[1].0,
            shared.efds[2].0,
            shared.efds[3].0,
        ];
        ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast::<RawFd>(), FDS);
    }
    let io = Io::SendMsg {
        fd: shared.control.as_raw_fd(),
        msg: p.cast(),
        flags: libc::MSG_NOSIGNAL as u32,
        zero_copy: false,
    };
    // SAFETY: the header and everything it points at live in the
    // `KernelBox`, which stays put when moved.
    let (res, _, _msg) = unsafe { Op::new(io, msg) }.await;
    if res < 0 {
        return Err(io::Error::from_raw_os_error(-res));
    }
    if res as usize != HANDSHAKE_LEN {
        return Err(handshake_failed("the handshake was cut short"));
    }
    drop(mem);
    let (res, buf) = shared.control.read(Vec::with_capacity(1)).await;
    match (res?, buf.first().copied()) {
        (1, Some(ACCEPTED)) => {}
        (1, Some(REFUSED_CAPACITY)) => {
            return Err(raise(
                Code::IpcCapacity,
                format!("the IPC listener refused a capacity of {capacity} bytes"),
                "connect with a smaller capacity, or raise the listener's limit in \
                 IpcListener::bind.",
            ))
        }
        (1, _) => return Err(handshake_failed("the listener refused the handshake")),
        _ => return Err(handshake_failed("the listener closed the connection")),
    }
    halves(shared, true)
}

/// How long a connecting process has to send its handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Handshakes a listener runs at once; further connections wait in the
/// socket's backlog.
const MAX_HANDSHAKES: usize = 64;

/// A handshake running as a task of its own.
type Handshake = JoinHandle<io::Result<(Sender, Receiver)>>;

/// Listens for [`connect`] on a Unix socket path.
pub struct IpcListener {
    inner: UnixListener,
    max_capacity: usize,
    /// Handshakes running or done, oldest first: each runs as a task of its
    /// own, so that a process that connects and sends nothing holds up no
    /// other.
    handshakes: RefCell<Vec<Handshake>>,
}

impl IpcListener {
    /// Listens at `path`, accepting channels of at most `max_capacity`
    /// bytes per direction (a power of two from 4 KiB to 1 GiB).
    ///
    /// The socket file's permissions decide who may connect, and it is
    /// created with the process's umask, so that by default any local user
    /// who can reach the path can open a channel and take handshake slots.
    /// Bind in a directory only the intended users can enter (mode `0700`
    /// for one user, or `0750` with a shared group), or narrow the file's
    /// mode before the first `accept`.
    pub fn bind<P: AsRef<Path>>(path: P, max_capacity: usize) -> io::Result<Self> {
        check_capacity(max_capacity, "IPC capacity limit")?;
        Ok(Self {
            inner: UnixListener::bind(path)?,
            max_capacity,
            handshakes: RefCell::new(Vec::new()),
        })
    }

    /// Waits for the next channel.
    ///
    /// Fails with `GIONG-E0092` or `GIONG-E0093` when a connecting process
    /// asks for too much or is not a giong IPC client; the listener keeps
    /// working.
    ///
    /// Handshakes run apart from the caller, up to 64 at once, and the
    /// first to finish is returned: a process that connects and sends
    /// nothing holds up no other, and fails with `GIONG-E0093` after 5
    /// seconds, its connection closed. So serve with a loop that goes on
    /// past such errors:
    ///
    /// ```no_run
    /// # giong::block_on(async {
    /// let listener = giong::ipc::IpcListener::bind("/run/app/ipc.sock", 1 << 20)?;
    /// loop {
    ///     let (tx, rx) = match listener.accept().await {
    ///         Ok(channel) => channel,
    ///         Err(e) if giong::error::code(&e).is_some() => continue, // one bad client
    ///         Err(e) => return Err(e),
    ///     };
    ///     giong::spawn(async move { /* serve tx, rx */ drop((tx, rx)) });
    /// }
    /// # Ok::<_, std::io::Error>(())
    /// # }).unwrap();
    /// ```
    ///
    /// Cancel safe: a handshake under way when the future is dropped goes
    /// on, and the next call returns it. Dropping the listener ends the
    /// handshakes under way, and those processes fail with `GIONG-E0093`.
    pub async fn accept(&self) -> io::Result<(Sender, Receiver)> {
        let mut incoming = Box::pin(self.inner.accept());
        poll_fn(|cx| loop {
            {
                let mut handshakes = self.handshakes.borrow_mut();
                for i in 0..handshakes.len() {
                    if let Poll::Ready(out) = Pin::new(&mut handshakes[i]).poll(cx) {
                        handshakes.remove(i);
                        return Poll::Ready(out.unwrap_or_else(|e| Err(lost_handshake(e))));
                    }
                }
                if handshakes.len() >= MAX_HANDSHAKES {
                    return Poll::Pending;
                }
            }
            match incoming.as_mut().poll(cx) {
                Poll::Ready(Ok(control)) => {
                    let max_capacity = self.max_capacity;
                    let task = crate::spawn(async move {
                        crate::time::timeout(HANDSHAKE_TIMEOUT, handshake(control, max_capacity))
                            .await
                            .unwrap_or_else(|_| Err(silent_client()))
                    });
                    self.handshakes.borrow_mut().push(task);
                    incoming.set(self.inner.accept());
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        })
        .await
    }
}

impl Drop for IpcListener {
    fn drop(&mut self) {
        for h in self.handshakes.get_mut().drain(..) {
            h.abort();
        }
    }
}

/// A process connected and sent no handshake in time.
fn silent_client() -> io::Error {
    raise(
        Code::IpcHandshake,
        format!(
            "IPC handshake failed: the connecting process sent no handshake within {} \
             seconds, so its connection was closed",
            HANDSHAKE_TIMEOUT.as_secs()
        ),
        "connect with giong::ipc::connect, which sends the handshake at once; check \
         which process opened the socket if none of yours did.",
    )
}

/// A handshake task that panicked or was cancelled.
fn lost_handshake(e: JoinError) -> io::Error {
    raise(
        Code::IpcHandshake,
        format!("IPC handshake failed: its task ended without an answer ({e})"),
        "report this as a giong bug, with the panic message from the log.",
    )
}

/// The listener's side of the handshake on a new connection.
async fn handshake(control: UnixStream, max_capacity: usize) -> io::Result<(Sender, Receiver)> {
    let msg = Received(handshake_msg());
    let io = Io::RecvMsg {
        fd: control.as_raw_fd(),
        msg: msg.0.as_ptr().cast(),
        // The memfd and the eventfds come in close-on-exec, as every
        // descriptor giong opens: none may leak into a program the
        // service runs.
        flags: libc::MSG_CMSG_CLOEXEC,
    };
    // SAFETY: as in `connect`.
    let (res, _, mut msg) = unsafe { Op::new(io, msg) }.await;
    if res < 0 {
        return Err(io::Error::from_raw_os_error(-res));
    }
    let fds = msg.take_fds();
    let m = msg.get();
    if res as usize != HANDSHAKE_LEN || fds.len() != FDS || m.hdr.msg_flags & libc::MSG_CTRUNC != 0
    {
        let _ = answer(&control, REFUSED_INVALID).await;
        return Err(handshake_failed("the handshake message is malformed"));
    }
    let magic = u64::from_le_bytes(m.data[..8].try_into().unwrap());
    let version = u32::from_le_bytes(m.data[8..12].try_into().unwrap());
    let capacity = u32::from_le_bytes(m.data[12..16].try_into().unwrap()) as usize;
    if magic != ring::MAGIC || version != ring::VERSION {
        let _ = answer(&control, REFUSED_INVALID).await;
        return Err(handshake_failed(
            "the client speaks another protocol or version",
        ));
    }
    if check_capacity(capacity, "IPC capacity").is_err() || capacity > max_capacity {
        let _ = answer(&control, REFUSED_CAPACITY).await;
        return Err(raise(
            Code::IpcCapacity,
            format!(
                "a client asked for an IPC capacity of {capacity} bytes, above this \
                 listener's limit of {}",
                max_capacity
            ),
            "have the client connect with a smaller capacity, or raise the limit in \
             IpcListener::bind.",
        ));
    }
    let len = ring::mapping_len(capacity);
    let mut fds = fds.into_iter();
    let mem = fds.next().unwrap();
    let efds: [Fd; 4] = [
        fds.next().unwrap(),
        fds.next().unwrap(),
        fds.next().unwrap(),
        fds.next().unwrap(),
    ];
    if let Err(why) =
        check_memfd(mem.0, len).and_then(|()| efds.iter().try_for_each(|fd| check_eventfd(fd.0)))
    {
        let _ = answer(&control, REFUSED_INVALID).await;
        return Err(handshake_failed(why));
    }
    let base = map(mem.0, len)?;
    drop(mem);
    let shared = Shared {
        base,
        len,
        capacity,
        efds,
        control,
    };
    // SAFETY: the mapping is at least a page long and page-aligned.
    let header = unsafe { &(*base.cast::<ring::Control>()).header };
    if header.magic.load(Ordering::Acquire) != ring::MAGIC
        || header.version.load(Ordering::Acquire) != ring::VERSION
        || header.capacity.load(Ordering::Acquire) as usize != capacity
    {
        let _ = answer(&shared.control, REFUSED_INVALID).await;
        return Err(handshake_failed(
            "the shared memory does not match the handshake",
        ));
    }
    answer(&shared.control, ACCEPTED).await?;
    halves(shared, false)
}

impl std::fmt::Debug for IpcListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcListener")
            .field("fd", &self.inner.as_raw_fd())
            .field("max_capacity", &self.max_capacity)
            .finish()
    }
}

/// The descriptors an `SCM_RIGHTS` message carried, owned (so closed if
/// the handshake fails).
/// A handshake message being received. The descriptors the kernel put in
/// its control buffer are ours from the moment it did: dropping the message
/// closes the ones [`Self::take_fds`] has not handed out. That covers a
/// receive that completes after its handshake was dropped (the listener went
/// away, or the client took too long), which the driver finishes on its own.
struct Received(KernelBox<HandshakeMsg>);

impl Received {
    fn get(&self) -> &HandshakeMsg {
        self.0.get()
    }

    /// Takes the descriptors out of the control buffer, once.
    fn take_fds(&mut self) -> Vec<Fd> {
        let mut out = Vec::new();
        let p = self.0.as_ptr();
        // SAFETY: the kernel is done with the box (its operation completed);
        // the control buffer is in it, and the CMSG macros stay inside
        // `msg_controllen`, which is then set to 0 so that nothing is taken
        // twice. A receive that never ran left the buffer zeroed, which holds
        // no SCM_RIGHTS entry.
        unsafe {
            let hdr = ptr::addr_of!((*p).hdr);
            let mut cmsg = libc::CMSG_FIRSTHDR(hdr);
            while !cmsg.is_null() && (*cmsg).cmsg_len as usize >= libc::CMSG_LEN(0) as usize {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let n = ((*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                        / std::mem::size_of::<RawFd>();
                    let data = libc::CMSG_DATA(cmsg).cast::<RawFd>();
                    for i in 0..n {
                        out.push(Fd(ptr::read_unaligned(data.add(i))));
                    }
                }
                cmsg = libc::CMSG_NXTHDR(hdr, cmsg);
            }
            (*p).hdr.msg_controllen = 0;
        }
        out
    }
}

impl Drop for Received {
    fn drop(&mut self) {
        drop(self.take_fds());
    }
}

fn check_memfd(fd: RawFd, len: usize) -> Result<(), &'static str> {
    // SAFETY: plain system calls on a descriptor we own.
    unsafe {
        let seals = libc::fcntl(fd, libc::F_GET_SEALS);
        if seals < 0 || seals & (libc::F_SEAL_SHRINK | libc::F_SEAL_GROW) == 0 {
            return Err("the shared memory is not sealed against resizing");
        }
        if seals & (libc::F_SEAL_SHRINK | libc::F_SEAL_GROW)
            != (libc::F_SEAL_SHRINK | libc::F_SEAL_GROW)
        {
            return Err("the shared memory is not sealed against resizing");
        }
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 || st.st_size as u64 != len as u64 {
            return Err("the shared memory has the wrong size");
        }
    }
    Ok(())
}

fn check_eventfd(fd: RawFd) -> Result<(), &'static str> {
    // Without /proc (some sandboxes) the check is skipped: a descriptor of
    // another kind only makes this channel's waits never end.
    match std::fs::read_link(format!("/proc/self/fd/{fd}")) {
        Ok(target) if target.as_os_str() != "anon_inode:[eventfd]" => {
            Err("a wake-up descriptor is not an eventfd")
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pipe whose write end goes over a socket pair into a [`Received`],
    /// as a client's descriptors do; returns the read end. The read end sees
    /// the end of the stream only once every copy of the write end is closed.
    fn receive_a_write_end() -> (Fd, Received) {
        let mut ends = [0; 2];
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        // SAFETY: plain system calls on descriptors this test owns; the
        // message and its buffers outlive the calls.
        unsafe {
            assert_eq!(
                libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK),
                0
            );
            let (read, write) = (Fd(ends[0]), Fd(ends[1]));
            let out = handshake_msg();
            let p = out.as_ptr();
            let fd_len = std::mem::size_of::<RawFd>() as u32;
            (*p).hdr.msg_controllen = libc::CMSG_SPACE(fd_len) as _;
            let cmsg = libc::CMSG_FIRSTHDR(ptr::addr_of!((*p).hdr));
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(fd_len) as _;
            libc::CMSG_DATA(cmsg)
                .cast::<RawFd>()
                .write_unaligned(write.0);
            let sent = libc::sendmsg(a.as_raw_fd(), ptr::addr_of!((*p).hdr), 0);
            assert_eq!(sent, HANDSHAKE_LEN as isize);
            drop(write);
            let msg = Received(handshake_msg());
            let got = libc::recvmsg(b.as_raw_fd(), msg.0.as_ptr().cast(), libc::MSG_CMSG_CLOEXEC);
            assert_eq!(got, HANDSHAKE_LEN as isize);
            (read, msg)
        }
    }

    fn at_end(read: &Fd) -> bool {
        let mut b = [0u8; 1];
        // SAFETY: a read into a live one-byte buffer.
        let n = unsafe { libc::read(read.0, b.as_mut_ptr().cast(), 1) };
        assert!(n == 0 || io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock);
        n == 0
    }

    #[test]
    fn a_received_message_nobody_took_closes_its_descriptors() {
        let (read, msg) = receive_a_write_end();
        assert!(!at_end(&read));
        drop(msg);
        assert!(at_end(&read));
    }

    #[test]
    fn descriptors_taken_from_a_message_outlive_it_and_close_once() {
        let (read, mut msg) = receive_a_write_end();
        let fds = msg.take_fds();
        assert_eq!(fds.len(), 1);
        // SAFETY: F_GETFD only reads the flags.
        let flags = unsafe { libc::fcntl(fds[0].0, libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, libc::FD_CLOEXEC);
        drop(msg);
        assert!(!at_end(&read));
        drop(fds);
        assert!(at_end(&read));
    }
}
