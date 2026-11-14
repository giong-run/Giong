//! Buffers: the ownership-passing buffer traits used by single-shot I/O, and the
//! kernel-shared buffer ring ("provided buffers") used by multishot receive.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::io;
use std::ops::Deref;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::task::Waker;

use io_uring::types::BufRingEntry;

use crate::error::{raise, Code};

/// A buffer that can be handed to the kernel for the duration of a write.
///
/// io_uring is completion based: the kernel keeps using the buffer until the
/// operation finishes, even if the future that started it is dropped. giong
/// therefore takes buffers *by value* and gives them back with the result.
///
/// # Safety
///
/// `as_ptr()` must point at `len()` initialised bytes that stay valid and at the
/// same address for as long as the value is alive, **even when the value itself
/// is moved** (heap or static storage — not an inline array).
pub unsafe trait IoBuf: 'static {
    /// Pointer to the first byte.
    fn as_ptr(&self) -> *const u8;
    /// Number of initialised bytes.
    fn len(&self) -> usize;
    /// `true` if there are no initialised bytes.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A buffer the kernel may write into (see [`IoBuf`] for the ownership model).
///
/// # Safety
///
/// In addition to the [`IoBuf`] contract, `as_mut_ptr()` must point at
/// `capacity()` writable bytes, and `set_init(n)` must make the first `n` bytes
/// the initialised contents.
pub unsafe trait IoBufMut: IoBuf {
    /// Pointer to the start of the writable region.
    fn as_mut_ptr(&mut self) -> *mut u8;
    /// Size of the writable region in bytes.
    fn capacity(&self) -> usize;
    /// Marks the first `n` bytes as initialised.
    ///
    /// # Safety
    ///
    /// The first `n` bytes must have been written.
    unsafe fn set_init(&mut self, n: usize);
}

unsafe impl IoBuf for Vec<u8> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        Vec::len(self)
    }
}

unsafe impl IoBufMut for Vec<u8> {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        <[u8]>::as_mut_ptr(self)
    }
    fn capacity(&self) -> usize {
        Vec::capacity(self)
    }
    unsafe fn set_init(&mut self, n: usize) {
        self.set_len(n);
    }
}

unsafe impl IoBuf for String {
    fn as_ptr(&self) -> *const u8 {
        str::as_ptr(self)
    }
    fn len(&self) -> usize {
        String::len(self)
    }
}

unsafe impl IoBuf for Box<[u8]> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

unsafe impl IoBuf for Rc<[u8]> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

unsafe impl IoBuf for Arc<[u8]> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

unsafe impl IoBuf for &'static [u8] {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

unsafe impl IoBuf for &'static str {
    fn as_ptr(&self) -> *const u8 {
        str::as_ptr(self)
    }
    fn len(&self) -> usize {
        str::len(self)
    }
}

// SAFETY: borrowed bytes are `'static`; owned ones live on the heap, which
// stays put when the `Cow` moves. Lets one vectored write mix the two.
unsafe impl IoBuf for Cow<'static, [u8]> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

/// An anonymous, private, zero-filled memory mapping.
///
/// Page aligned (as a buffer ring must be) and lazily backed: pages only
/// become resident when first touched, so an idle worker's receive pool
/// costs address space, not memory.
struct Mapping {
    ptr: NonNull<u8>,
    len: usize,
}

impl Mapping {
    fn new(len: usize) -> io::Result<Self> {
        // SAFETY: a fresh anonymous mapping; the result is checked.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            ptr: NonNull::new(ptr as *mut u8).expect("mmap returned null"),
            len,
        })
    }
}

fn page_size() -> usize {
    // SAFETY: plain libc call without pointers.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).unwrap_or(4096)
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: exactly the region mapped in `new`.
        unsafe { libc::munmap(self.ptr.as_ptr() as *mut libc::c_void, self.len) };
    }
}

/// One of the two io_uring buffer rings a [`BufPool`] can lend its buffers
/// through. The value is the ring's buffer group id.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RingId {
    /// Every receive takes a whole buffer, however few bytes it brings.
    /// Datagrams always use this ring, and streams do where the kernel has
    /// no packed ring.
    Whole = 0,
    /// Linux 6.12 and newer (`IOU_PBUF_RING_INC`): a receive takes only the
    /// bytes it brings, and the next one continues in the same buffer. A
    /// hundred small requests then fill one buffer instead of touching a
    /// hundred. Streams only: a datagram could be cut where a buffer ends.
    Packed = 1,
}

impl RingId {
    /// The buffer group id the ring is registered under.
    pub(crate) const fn group(self) -> u16 {
        self as u16
    }

    const fn other(self) -> RingId {
        match self {
            RingId::Whole => RingId::Packed,
            RingId::Packed => RingId::Whole,
        }
    }
}

/// One buffer ring shared with the kernel.
struct Ring {
    /// The ring entries.
    map: Mapping,
    /// Our private copy of the ring tail (the published copy lives in `map`).
    tail: Cell<u16>,
    /// Buffers in the ring: the kernel may write to them.
    published: Cell<usize>,
    /// How many free buffers to keep in the ring. 0 until the ring is first
    /// used, and always with the epoll driver, which takes buffers from
    /// `spare` itself.
    target: Cell<usize>,
    /// A receive found the ring empty since the last refill.
    ran_dry: Cell<bool>,
    /// The io_uring driver registered the ring with the kernel.
    registered: Cell<bool>,
}

impl Ring {
    fn new(count: u16) -> io::Result<Self> {
        // One page more than the entries need: Linux 6.0 (and 5.19) sizes the
        // ring as `struct_size(br, bufs, count)`, 16 bytes more than `count`
        // entries, so for 256 entries or more it pins one page past the end
        // and fails with EFAULT unless that page happens to be mapped.
        // Fixed in 6.1 ("io_uring: fix size calculation when registering buf
        // ring"); found by running the tests on a real 6.0 kernel.
        let len = count as usize * std::mem::size_of::<BufRingEntry>() + page_size();
        Ok(Self {
            map: Mapping::new(len)?,
            tail: Cell::new(0),
            published: Cell::new(0),
            target: Cell::new(0),
            ran_dry: Cell::new(false),
            registered: Cell::new(false),
        })
    }

    fn entries(&self) -> *mut BufRingEntry {
        self.map.ptr.as_ptr() as *mut BufRingEntry
    }
}

/// What the pool knows about one of its buffers.
struct Slot {
    /// Bytes the kernel has put into the buffer since it was last lent to
    /// the packed ring: where the next piece starts.
    filled: Cell<u32>,
    /// Pieces of the buffer that user space holds: [`RecvBuf`]s, and
    /// completions the driver has queued but nobody has read yet.
    pieces: Cell<u16>,
    /// The ring the buffer is lent to, if any: the kernel may write to it.
    lent_to: Cell<Option<RingId>>,
    /// The ring it was last lent to, where it goes back first.
    home: Cell<RingId>,
}

/// The per-worker pool of receive buffers.
///
/// With io_uring the pool lends buffers to the kernel through *buffer rings*
/// (`IORING_REGISTER_PBUF_RING`): when a multishot receive has data, the
/// kernel picks a buffer from a ring, fills it and names it in the
/// completion. The epoll driver takes free buffers itself
/// ([`next_free`](Self::next_free)).
///
/// A buffer is free when no ring holds it and no piece of it is held by user
/// space. Free buffers that are not in a ring wait in `spare`, the most
/// recently used on top.
pub(crate) struct BufPool {
    /// `count * buf_len` bytes of buffer space.
    mem: Mapping,
    count: u16,
    buf_len: usize,
    /// Indexed by [`RingId`].
    rings: [Ring; 2],
    /// Indexed by buffer id.
    slots: Box<[Slot]>,
    /// Free buffers that are not in a ring, the most recently used on top.
    ///
    /// The kernel takes ring entries in order and every buffer given back
    /// goes to the end of the ring, so a ring holding all `count` buffers
    /// cycles through every one of them and makes all of their memory
    /// resident, however few are in use at a time. Keeping only `target` in
    /// a ring and the rest here, reused last-in first-out, keeps the memory
    /// a worker touches close to what its traffic needs.
    spare: RefCell<Vec<u16>>,
    /// Tasks waiting for a buffer to come back.
    waiters: RefCell<Vec<Waker>>,
}

/// Buffers an io_uring worker keeps in a ring at first. The number doubles,
/// up to the pool's size, whenever a batch of completions takes most of them
/// or the kernel finds the ring empty.
const FIRST_TARGET: usize = 16;

/// What [`BufPool::received`] makes of a receive completion.
pub(crate) enum Received {
    /// `len` bytes from offset `off` of buffer `bid`, now held by user
    /// space as one piece: hand it out, or give it back with
    /// [`BufPool::release`].
    Data { bid: u16, off: u32, len: u32 },
    /// No data. If the completion named a buffer, it is back in the pool.
    Nothing,
}

impl BufPool {
    pub(crate) fn new(count: u16, buf_len: usize) -> io::Result<Rc<Self>> {
        if count == 0 || !count.is_power_of_two() || count > 32768 {
            return Err(raise(
                Code::InvalidConfig,
                format!(
                    "the recv buffer count must be a power of two from 1 to 32768, not {count}"
                ),
                "pass a count in that range to Builder::recv_buffers.",
            ));
        }
        if buf_len == 0 || buf_len > u32::MAX as usize {
            return Err(raise(
                Code::InvalidConfig,
                format!("the recv buffer size must be from 1 byte to 4 GiB, not {buf_len}"),
                "pass a size in that range to Builder::recv_buffers.",
            ));
        }
        let mem_size = (count as usize).checked_mul(buf_len).ok_or_else(|| {
            raise(
                Code::InvalidConfig,
                format!("{count} recv buffers of {buf_len} bytes do not fit in memory"),
                "pass fewer or smaller buffers to Builder::recv_buffers.",
            )
        })?;
        let mem = Mapping::new(mem_size)?;
        let slots = (0..count)
            .map(|_| Slot {
                filled: Cell::new(0),
                pieces: Cell::new(0),
                lent_to: Cell::new(None),
                home: Cell::new(RingId::Whole),
            })
            .collect();

        let pool = Self {
            mem,
            count,
            buf_len,
            rings: [Ring::new(count)?, Ring::new(count)?],
            slots,
            // Buffer 0 on top: the lowest addresses are used first.
            spare: RefCell::new((0..count).rev().collect()),
            waiters: RefCell::new(Vec::new()),
        };
        Ok(Rc::new(pool))
    }

    fn ring(&self, id: RingId) -> &Ring {
        &self.rings[id as usize]
    }

    fn slot(&self, bid: u16) -> &Slot {
        assert!(bid < self.count, "kernel returned an invalid buffer");
        &self.slots[bid as usize]
    }

    /// Address of a ring's entries, to register it with the kernel.
    pub(crate) fn ring_addr(&self, id: RingId) -> u64 {
        self.ring(id).map.ptr.as_ptr() as u64
    }

    /// The io_uring driver registered ring `id`. It starts lending buffers
    /// the first time a receive needs one ([`ensure_published`](Self::ensure_published)).
    pub(crate) fn set_registered(&self, id: RingId) {
        self.ring(id).registered.set(true);
    }

    /// `true` if the io_uring driver registered ring `id`.
    pub(crate) fn is_registered(&self, id: RingId) -> bool {
        self.ring(id).registered.get()
    }

    /// The most buffers ring `id` may hold. While both rings are in use each
    /// gets at most half the pool, so that one ring cannot keep every buffer
    /// while receives on the other wait for one.
    fn max_target(&self, id: RingId) -> usize {
        let count = self.count as usize;
        if self.ring(id.other()).target.get() == 0 {
            count
        } else {
            (count / 2).max(1)
        }
    }

    /// The first receive through ring `id`: start lending it buffers.
    fn start(&self, id: RingId) {
        let ring = self.ring(id);
        if ring.target.get() != 0 || !ring.registered.get() {
            return;
        }
        let other = self.ring(id.other());
        if other.target.get() != 0 {
            // Share the pool from now on; the other ring gives buffers back
            // to the spare ones until it is down to its half.
            other
                .target
                .set(other.target.get().min((self.count as usize / 2).max(1)));
        }
        let target = FIRST_TARGET.min(self.max_target(id));
        ring.target.set(target);
        self.fill_to(id, target);
    }

    /// Tops each ring up to its target from the spare buffers once it is
    /// half empty. The io_uring driver calls it after every batch of
    /// completions, before the kernel can take more. If the kernel found a
    /// ring empty during the batch, or the batch took three quarters of it,
    /// its target doubles first (once per batch, however many receives ran
    /// dry).
    pub(crate) fn refill(&self) {
        for id in [RingId::Whole, RingId::Packed] {
            let ring = self.ring(id);
            let target = ring.target.get();
            if target == 0 {
                continue;
            }
            let ran_dry = ring.ran_dry.replace(false);
            let published = ring.published.get();
            if ran_dry || published * 4 < target {
                ring.target.set((target * 2).min(self.max_target(id)));
            } else if published * 2 >= target {
                // Plenty left: the buffers this batch took go straight back
                // into the ring when their readers drop them.
                continue;
            }
            self.fill_to(id, ring.target.get());
        }
    }

    /// The kernel found ring `id` empty: the next [`refill`](Self::refill)
    /// keeps more buffers in it.
    pub(crate) fn ran_dry(&self, id: RingId) {
        self.ring(id).ran_dry.set(true);
    }

    fn fill_to(&self, id: RingId, target: usize) {
        let ring = self.ring(id);
        let mut spare = self.spare.borrow_mut();
        let before = ring.published.get();
        while ring.published.get() < target {
            let Some(bid) = spare.pop() else { break };
            self.push(id, bid);
        }
        drop(spare);
        if ring.published.get() != before {
            self.publish_tail(id);
        }
    }

    pub(crate) fn count(&self) -> u16 {
        self.count
    }

    /// Buffers not in use: in a ring (even part-filled in the packed one) or
    /// spare.
    pub(crate) fn free(&self) -> usize {
        self.rings.iter().map(|r| r.published.get()).sum::<usize>() + self.spare.borrow().len()
    }

    /// Makes sure ring `ring` holds a buffer before a receive through it is
    /// armed (`None`: the epoll driver, which needs a spare one). Returns
    /// `false` if there is none.
    pub(crate) fn ensure_published(&self, ring: Option<RingId>) -> bool {
        match ring {
            Some(id) if self.ring(id).registered.get() => {
                self.start(id);
                let r = self.ring(id);
                if r.published.get() == 0 {
                    self.fill_to(id, 1);
                }
                r.published.get() > 0
            }
            _ => !self.spare.borrow().is_empty(),
        }
    }

    /// `true` if every free buffer is in a ring.
    pub(crate) fn no_spare(&self) -> bool {
        self.spare.borrow().is_empty()
    }

    /// Accounts for a receive completion through ring `id` that names buffer
    /// `bid`, with result `res` (bytes, or an error). `more` is
    /// `IORING_CQE_F_BUF_MORE`: the packed ring keeps the rest of the buffer
    /// for the next receive.
    ///
    /// Call it for every such completion, in the order the kernel posted
    /// them: in the packed ring that order is what says where each piece
    /// starts.
    pub(crate) fn received(&self, id: RingId, bid: u16, res: i32, more: bool) -> Received {
        let slot = self.slot(bid);
        match id {
            RingId::Whole => {
                // The kernel took the buffer out of the ring, whatever it
                // put in it.
                self.taken(id, slot);
                if res <= 0 {
                    self.recycle(bid);
                    return Received::Nothing;
                }
                debug_assert_eq!(slot.pieces.get(), 0);
                slot.pieces.set(1);
                Received::Data {
                    bid,
                    off: 0,
                    len: res as u32,
                }
            }
            RingId::Packed => {
                // Without data the kernel commits nothing: the buffer stays
                // where it was, at the head of the ring.
                if res <= 0 {
                    return Received::Nothing;
                }
                let off = slot.filled.get();
                let len = res as u32;
                assert!(
                    off as usize + len as usize <= self.buf_len,
                    "kernel returned more data than a buffer holds"
                );
                slot.filled.set(off + len);
                slot.pieces.set(slot.pieces.get() + 1);
                if !more {
                    // Full: the kernel has moved on to the next buffer.
                    self.taken(id, slot);
                }
                Received::Data { bid, off, len }
            }
        }
    }

    fn taken(&self, id: RingId, slot: &Slot) {
        let ring = self.ring(id);
        ring.published.set(ring.published.get().saturating_sub(1));
        slot.lent_to.set(None);
    }

    /// The epoll driver: the buffer to receive into next (the most recently
    /// used free one), as its id and its memory, or `None` if every buffer
    /// is in use. It stays free until [`take_next`](Self::take_next).
    pub(crate) fn next_free(&self) -> Option<(u16, *mut u8, usize)> {
        let bid = *self.spare.borrow().last()?;
        debug_assert!(bid < self.count);
        // SAFETY: `bid < count`, so the buffer lies inside `mem`.
        let ptr = unsafe { self.mem.ptr.as_ptr().add(bid as usize * self.buf_len) };
        Some((bid, ptr, self.buf_len))
    }

    /// The epoll driver: the buffer [`next_free`](Self::next_free) returned
    /// now holds data and belongs to user space as one piece until it is
    /// released.
    pub(crate) fn take_next(&self) {
        let taken = self.spare.borrow_mut().pop();
        debug_assert!(taken.is_some());
        if let Some(bid) = taken {
            self.slot(bid).pieces.set(1);
        }
    }

    /// User space gives back one piece of buffer `bid`. The buffer is free
    /// again once no piece is held and no ring holds it.
    pub(crate) fn release(&self, bid: u16) {
        let slot = self.slot(bid);
        let pieces = slot.pieces.get();
        debug_assert!(
            pieces > 0,
            "buffer {bid} released more often than handed out"
        );
        slot.pieces.set(pieces.saturating_sub(1));
        if pieces <= 1 && slot.lent_to.get().is_none() {
            self.recycle(bid);
        }
    }

    /// A buffer is free: back to the ring it came from if that one is below
    /// its target, else to the spare buffers. Wakes tasks starved for
    /// buffers.
    fn recycle(&self, bid: u16) {
        let slot = self.slot(bid);
        slot.filled.set(0);
        let home = slot.home.get();
        let ring = self.ring(home);
        if ring.published.get() < ring.target.get() {
            self.push(home, bid);
            self.publish_tail(home);
        } else {
            self.spare.borrow_mut().push(bid);
        }
        if !self.waiters.borrow().is_empty() {
            let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
            for waker in waiters {
                waker.wake();
            }
        }
    }

    /// Registers a task to be woken when a buffer is recycled.
    pub(crate) fn wait(&self, waker: &Waker) {
        let mut waiters = self.waiters.borrow_mut();
        if !waiters.iter().any(|w| w.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }

    /// Lends buffer `bid` to ring `id`. The kernel sees it once the tail is
    /// published.
    fn push(&self, id: RingId, bid: u16) {
        let ring = self.ring(id);
        let slot = self.slot(bid);
        let tail = ring.tail.get();
        let idx = (tail & (self.count - 1)) as usize;
        // SAFETY: `idx < count`, the ring allocation holds `count` entries,
        // and the kernel only reads entries below the published tail. The
        // setters do not touch the `resv` field, which doubles as the ring
        // tail in entry 0.
        unsafe {
            let entry = &mut *ring.entries().add(idx);
            entry.set_addr(self.mem.ptr.as_ptr().add(bid as usize * self.buf_len) as u64);
            entry.set_len(self.buf_len as u32);
            entry.set_bid(bid);
        }
        ring.tail.set(tail.wrapping_add(1));
        ring.published.set(ring.published.get() + 1);
        slot.filled.set(0);
        slot.lent_to.set(Some(id));
        slot.home.set(id);
    }

    fn publish_tail(&self, id: RingId) {
        let ring = self.ring(id);
        // SAFETY: the tail lives inside the ring allocation and is only ever
        // accessed atomically (the kernel reads it concurrently).
        unsafe {
            let tail = BufRingEntry::tail(ring.entries()) as *mut u16;
            AtomicU16::from_ptr(tail).store(ring.tail.get(), Ordering::Release);
        }
    }

    /// Copies a piece of a buffer to the heap (used to free pool buffers that
    /// nobody is reading from).
    pub(crate) fn copy_out(&self, bid: u16, off: u32, len: u32) -> Vec<u8> {
        self.slice(bid, off, len).to_vec()
    }

    fn slice(&self, bid: u16, off: u32, len: u32) -> &[u8] {
        let (off, len) = (off as usize, len as usize);
        assert!(
            bid < self.count && off + len <= self.buf_len,
            "kernel returned an invalid buffer"
        );
        // SAFETY: in bounds per the assert; the kernel wrote these bytes and
        // writes to this buffer only past them (the packed ring) or not at
        // all until it is recycled.
        unsafe {
            std::slice::from_raw_parts(
                self.mem.ptr.as_ptr().add(bid as usize * self.buf_len + off),
                len,
            )
        }
    }
}

// The mappings are released when the pool is dropped. The runtime only lets
// the last reference go once the ring that used this memory is quiesced.

/// Bytes received by [`TcpStream::recv`](crate::net::TcpStream::recv).
///
/// The data normally lives in a buffer borrowed from the worker's
/// kernel-shared pool; dropping the `RecvBuf` returns its part of the buffer
/// to the pool. Copy out what you need and drop it promptly — do not hold one
/// across an `.await`, or slow peers can starve the pool (where buffers are
/// packed, it also holds back the other connections' data in that buffer).
///
/// A stream has no message boundaries: one write by the peer may arrive in
/// several `RecvBuf`s, and several writes in one.
///
/// An empty `RecvBuf` means the peer closed the connection.
pub struct RecvBuf {
    inner: Inner,
}

enum Inner {
    /// A piece of a pool buffer: `(pool, buffer id, offset, length)`.
    Pooled(Rc<BufPool>, u16, u32, u32),
    /// Data the driver moved to the heap because the pool was under pressure.
    Heap(Vec<u8>),
    Eof,
}

impl RecvBuf {
    /// A piece of pool buffer `bid` that the pool counts as held (see
    /// [`BufPool::received`]); dropping the `RecvBuf` releases it.
    pub(crate) fn pooled(pool: Rc<BufPool>, bid: u16, off: u32, len: u32) -> Self {
        Self {
            inner: Inner::Pooled(pool, bid, off, len),
        }
    }

    pub(crate) fn heap(data: Vec<u8>) -> Self {
        Self {
            inner: Inner::Heap(data),
        }
    }

    pub(crate) fn eof() -> Self {
        Self { inner: Inner::Eof }
    }

    /// The received bytes.
    pub fn as_slice(&self) -> &[u8] {
        match &self.inner {
            Inner::Pooled(pool, bid, off, len) => pool.slice(*bid, *off, *len),
            Inner::Heap(data) => data,
            Inner::Eof => &[],
        }
    }
}

impl Deref for RecvBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsRef<[u8]> for RecvBuf {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::fmt::Debug for RecvBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecvBuf")
            .field("len", &self.as_slice().len())
            .finish()
    }
}

impl Drop for RecvBuf {
    fn drop(&mut self) {
        if let Inner::Pooled(pool, bid, ..) = &self.inner {
            pool.release(*bid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's view of a ring is simulated: these tests call
    /// `received` the way the io_uring driver does for each completion.
    fn pool(count: u16, len: usize) -> Rc<BufPool> {
        let pool = BufPool::new(count, len).unwrap();
        pool.set_registered(RingId::Whole);
        pool.set_registered(RingId::Packed);
        pool
    }

    fn data(r: Received) -> (u16, u32, u32) {
        match r {
            Received::Data { bid, off, len } => (bid, off, len),
            Received::Nothing => panic!("expected data"),
        }
    }

    #[test]
    fn packed_pieces_follow_each_other_and_free_the_buffer_last() {
        let pool = pool(4, 100);
        assert!(pool.ensure_published(Some(RingId::Packed)));
        assert_eq!(pool.free(), 4);

        let a = data(pool.received(RingId::Packed, 0, 30, true));
        let b = data(pool.received(RingId::Packed, 0, 30, true));
        assert_eq!((a, b), ((0, 0, 30), (0, 30, 30)));
        // Still the kernel's: it keeps filling it.
        assert_eq!(pool.free(), 4);
        // Without data the kernel commits nothing.
        assert!(matches!(
            pool.received(RingId::Packed, 0, 0, false),
            Received::Nothing
        ));
        assert!(matches!(
            pool.received(RingId::Packed, 0, -104, false),
            Received::Nothing
        ));
        let c = data(pool.received(RingId::Packed, 0, 40, false));
        assert_eq!(c, (0, 60, 40));
        assert_eq!(pool.free(), 3, "full: the kernel moved on");

        // Pieces given back in any order; the buffer is free with the last.
        pool.release(0);
        pool.release(0);
        assert_eq!(pool.free(), 3);
        pool.release(0);
        assert_eq!(pool.free(), 4);

        // Refilled from the start.
        let d = data(pool.received(RingId::Packed, 1, 10, true));
        assert_eq!(d, (1, 0, 10));
        pool.release(1);
        assert_eq!(pool.free(), 4, "a part-filled buffer stays with the kernel");
    }

    #[test]
    fn a_piece_released_while_the_kernel_fills_the_buffer_does_not_free_it() {
        let pool = pool(2, 64);
        assert!(pool.ensure_published(Some(RingId::Packed)));
        let a = data(pool.received(RingId::Packed, 0, 10, true));
        pool.release(a.0);
        let b = data(pool.received(RingId::Packed, 0, 54, false));
        assert_eq!(b, (0, 10, 54));
        assert_eq!(pool.free(), 1);
        pool.release(b.0);
        assert_eq!(pool.free(), 2);
    }

    #[test]
    fn whole_buffers_are_taken_by_every_completion() {
        let pool = pool(4, 64);
        assert!(pool.ensure_published(Some(RingId::Whole)));
        let a = data(pool.received(RingId::Whole, 0, 5, false));
        assert_eq!(a, (0, 0, 5));
        assert_eq!(pool.free(), 3);
        // End of stream with a buffer: it comes straight back.
        assert!(matches!(
            pool.received(RingId::Whole, 1, 0, false),
            Received::Nothing
        ));
        assert_eq!(pool.free(), 3);
        pool.release(0);
        assert_eq!(pool.free(), 4);
    }

    #[test]
    fn two_rings_share_the_pool() {
        let pool = pool(8, 64);
        assert!(pool.ensure_published(Some(RingId::Packed)));
        // The packed ring took all eight (its first target is 16).
        assert!(pool.no_spare());
        // The whole-buffer ring starts: the packed one is cut to half the
        // pool, and gives back buffers as they come free.
        assert!(!pool.ensure_published(Some(RingId::Whole)));
        for bid in 0..4 {
            let piece = data(pool.received(RingId::Packed, bid, 64, false));
            pool.release(piece.0);
        }
        assert!(pool.ensure_published(Some(RingId::Whole)));
        pool.refill();
        assert_eq!(pool.free(), 8);
        // Each ring now holds at most half.
        assert_eq!(pool.rings[RingId::Packed as usize].published.get(), 4);
        assert_eq!(pool.rings[RingId::Whole as usize].published.get(), 4);
    }

    #[test]
    fn epoll_takes_spare_buffers_and_gets_them_back() {
        let pool = BufPool::new(2, 64).unwrap();
        assert!(pool.ensure_published(None));
        let (bid, _, len) = pool.next_free().unwrap();
        assert_eq!(len, 64);
        pool.take_next();
        assert_eq!(pool.free(), 1);
        pool.release(bid);
        assert_eq!(pool.free(), 2);
        assert_eq!(pool.next_free().unwrap().0, bid, "most recently used first");
    }
}
