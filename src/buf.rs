//! Buffers: the ownership-passing buffer traits used by single-shot I/O, and the
//! kernel-shared buffer ring ("provided buffers") used by multishot receive.

// The buffer rings are io_uring's: elsewhere the pool only lends buffers
// to the readiness driver, and the ring bookkeeping goes unused.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::io;
use std::ops::Deref;
use std::ptr::{self, NonNull};
use std::rc::Rc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::task::Waker;

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
/// is moved** (heap or static storage — not an inline array). While giong holds
/// the value (from the call that takes it until the value comes back), the
/// bytes must not be written except through it: the kernel may be reading them.
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
/// the initialised contents. While giong holds the value, those bytes must be
/// neither read nor written except through it: the kernel may be writing them.
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

// The bytes of every type below are on the heap or in static memory, neither
// of which moves when the value moves, and none of these types lets safe code
// change the bytes while an operation owns the value.

// SAFETY: a `Vec`'s bytes are on the heap; only its owner can change them.
unsafe impl IoBuf for Vec<u8> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        Vec::len(self)
    }
}

// SAFETY: the allocation holds `capacity` bytes on the heap; `set_init`
// only marks bytes the kernel wrote.
unsafe impl IoBufMut for Vec<u8> {
    fn as_mut_ptr(&mut self) -> *mut u8 {
        // The inherent method, whose pointer is valid for the whole
        // allocation: the kernel writes up to `capacity`, past `len`. (A
        // pointer from the slice, `<[u8]>::as_mut_ptr`, covers only `len`
        // bytes, usually none.)
        Vec::as_mut_ptr(self)
    }
    fn capacity(&self) -> usize {
        Vec::capacity(self)
    }
    unsafe fn set_init(&mut self, n: usize) {
        self.set_len(n);
    }
}

// SAFETY: a `String`'s bytes are on the heap; only its owner can change them.
unsafe impl IoBuf for String {
    fn as_ptr(&self) -> *const u8 {
        str::as_ptr(self)
    }
    fn len(&self) -> usize {
        String::len(self)
    }
}

// SAFETY: a `Box`'s bytes are on the heap; only its owner can change them.
unsafe impl IoBuf for Box<[u8]> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

// SAFETY: on the heap, and immutable while shared (`Rc::get_mut` needs the
// only reference, and the operation holds one).
unsafe impl IoBuf for Rc<[u8]> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

// SAFETY: on the heap, and immutable while shared (`Arc::get_mut` needs the
// only reference, and the operation holds one).
unsafe impl IoBuf for Arc<[u8]> {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

// SAFETY: borrowed immutably for `'static`, so nothing can change the bytes.
unsafe impl IoBuf for &'static [u8] {
    fn as_ptr(&self) -> *const u8 {
        <[u8]>::as_ptr(self)
    }
    fn len(&self) -> usize {
        <[u8]>::len(self)
    }
}

// SAFETY: borrowed immutably for `'static`, so nothing can change the bytes.
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

/// Whether nobody can change a buffer's bytes once a write has given the
/// buffer back: what lets a zero-copy send return before the kernel stops
/// reading them (see `StreamSocket::zero_copy_report`).
pub(crate) enum Sharing {
    /// The bytes may change once the buffer is back (`Vec`, `Box`,
    /// `String`, an owned `Cow`, any other buffer type).
    No,
    /// `'static` bytes, which never change.
    Static,
    /// Reference-counted bytes (`Rc<[u8]>`, `Arc<[u8]>`): this second owner
    /// keeps them alive, and while it lives nobody can get them mutably.
    Owner(Box<dyn std::any::Any>),
}

/// The [`Sharing`] of `buf`, without making an owner.
pub(crate) fn is_shared<B: IoBuf>(buf: &B) -> bool {
    let any: &dyn std::any::Any = buf;
    any.is::<&'static [u8]>()
        || any.is::<&'static str>()
        || any.is::<Rc<[u8]>>()
        || any.is::<Arc<[u8]>>()
        || matches!(
            any.downcast_ref::<Cow<'static, [u8]>>(),
            Some(Cow::Borrowed(_))
        )
}

/// The [`Sharing`] of `buf`.
pub(crate) fn sharing<B: IoBuf>(buf: &B) -> Sharing {
    let any: &dyn std::any::Any = buf;
    if let Some(rc) = any.downcast_ref::<Rc<[u8]>>() {
        Sharing::Owner(Box::new(Rc::clone(rc)))
    } else if let Some(arc) = any.downcast_ref::<Arc<[u8]>>() {
        Sharing::Owner(Box::new(Arc::clone(arc)))
    } else if is_shared(buf) {
        Sharing::Static
    } else {
        Sharing::No
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

#[cfg(not(kani))]
impl Mapping {
    fn new(len: usize) -> io::Result<Self> {
        let Some(ptr) = NonNull::new(crate::sys::map_pool(len)) else {
            return Err(io::Error::last_os_error());
        };
        Ok(Self { ptr, len })
    }
}

/// Under Kani: heap memory instead of a mapping, which Kani cannot model
/// (`mmap` is a foreign function). Zero-filled as a fresh mapping is.
#[cfg(kani)]
impl Mapping {
    const ALIGN: usize = 8;

    fn new(len: usize) -> io::Result<Self> {
        Ok(Self::heap(len))
    }

    fn heap(len: usize) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, Self::ALIGN).unwrap();
        // SAFETY: `len` is never 0 (the pool checks its sizes).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        Self {
            ptr: NonNull::new(ptr).expect("allocation failed"),
            len,
        }
    }
}

#[cfg(not(kani))]
use crate::sys::page_size;

/// Under Kani: a small stand-in, only the size of the padding `Ring::new`
/// adds for Linux 6.0 (`sysconf` is a foreign function).
#[cfg(kani)]
fn page_size() -> usize {
    16
}

#[cfg(not(kani))]
impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: exactly the region mapped in `new`, which this value owns.
        unsafe { crate::sys::unmap_pool(self.ptr.as_ptr(), self.len) };
    }
}

#[cfg(kani)]
impl Drop for Mapping {
    fn drop(&mut self) {
        let layout = std::alloc::Layout::from_size_align(self.len, Self::ALIGN).unwrap();
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), layout) };
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
        Ok(Self::with_map(Mapping::new(Self::size(count))?))
    }

    fn size(count: u16) -> usize {
        count as usize * std::mem::size_of::<RawEntry>() + page_size()
    }

    fn with_map(map: Mapping) -> Self {
        Self {
            map,
            tail: Cell::new(0),
            published: Cell::new(0),
            target: Cell::new(0),
            ran_dry: Cell::new(false),
            registered: Cell::new(false),
        }
    }

    fn entries(&self) -> *mut RawEntry {
        self.map.ptr.as_ptr() as *mut RawEntry
    }
}

/// The layout of the kernel's `struct io_uring_buf`, one entry of a buffer
/// ring, for writing an entry field by field through raw pointers. `resv` of
/// entry 0 is the ring's tail, which the kernel reads at any time, so a
/// reference to an entry (which `io_uring::types::BufRingEntry`'s setters
/// take) would claim memory the kernel shares.
#[repr(C)]
struct RawEntry {
    addr: u64,
    len: u32,
    bid: u16,
    resv: u16,
}

#[cfg(target_os = "linux")]
const _: () = assert!(
    std::mem::size_of::<RawEntry>() == std::mem::size_of::<io_uring::types::BufRingEntry>()
);

/// What the pool knows about one of its buffers.
struct Slot {
    /// Bytes the kernel has put into the buffer since it was last lent to
    /// the packed ring: where the next piece starts.
    filled: Cell<u32>,
    /// Pieces of the buffer that user space holds: [`RecvBuf`]s, and
    /// completions the driver has queued but nobody has read yet. Every
    /// piece of a packed buffer holds at least one byte, so there are at
    /// most `buf_len` (below 4 GiB) of them: a `u16` wrapped once 65,536
    /// one-byte pieces of a buffer over 64 KiB were held, and the buffer
    /// went back to the kernel under them.
    pieces: Cell<u32>,
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
        let rings = [Ring::new(count)?, Ring::new(count)?];
        Ok(Rc::new(Self::with_memory(count, buf_len, mem, rings)))
    }

    /// A pool of `count` buffers of `buf_len` bytes in `mem`, sizes checked.
    fn with_memory(count: u16, buf_len: usize, mem: Mapping, rings: [Ring; 2]) -> Self {
        let slots = (0..count)
            .map(|_| Slot {
                filled: Cell::new(0),
                pieces: Cell::new(0),
                lent_to: Cell::new(None),
                home: Cell::new(RingId::Whole),
            })
            .collect();

        Self {
            mem,
            count,
            buf_len,
            rings,
            slots,
            // Buffer 0 on top: the lowest addresses are used first.
            spare: RefCell::new((0..count).rev().collect()),
            waiters: RefCell::new(Vec::new()),
        }
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

    /// The size of every buffer, in bytes.
    #[cfg_attr(not(feature = "unstable-udp-offload"), allow(dead_code))]
    pub(crate) fn buf_len(&self) -> usize {
        self.buf_len
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
                // Cannot overflow (see `Slot::pieces`); checked anyway,
                // because a wrapped count frees a buffer still in use.
                let pieces = slot.pieces.get().checked_add(1);
                slot.pieces
                    .set(pieces.expect("giong: pieces of a receive buffer overflowed"));
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
        // Released more often than handed out, the buffer would go back to
        // the kernel while pieces of it are still read: stop instead.
        assert!(
            pieces > 0,
            "giong: buffer {bid} released more often than handed out"
        );
        slot.pieces.set(pieces - 1);
        if pieces == 1 && slot.lent_to.get().is_none() {
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
        // fields are written through raw pointers, never through a reference
        // to the entry, and `resv` (the ring tail in entry 0) is not touched.
        unsafe {
            let entry = ring.entries().add(idx);
            let addr = self.mem.ptr.as_ptr().add(bid as usize * self.buf_len) as u64;
            ptr::addr_of_mut!((*entry).addr).write(addr);
            ptr::addr_of_mut!((*entry).len).write(self.buf_len as u32);
            ptr::addr_of_mut!((*entry).bid).write(bid);
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
            let tail = std::ptr::addr_of_mut!((*ring.entries()).resv);
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

    /// A buffer over 64 KiB can be cut into more than 65,535 one-byte
    /// pieces. Their count used to be a `u16`: at 65,536 held pieces it
    /// wrapped to 0, the buffer went back to the kernel while every piece
    /// still pointed into it, and the next receive overwrote them.
    #[test]
    fn a_buffer_cut_into_more_than_65535_pieces_is_freed_with_the_last() {
        const LEN: usize = 128 * 1024;
        const PIECES: u32 = 70_000;
        let pool = pool(2, LEN);
        assert!(pool.ensure_published(Some(RingId::Packed)));
        for i in 0..PIECES {
            assert_eq!(data(pool.received(RingId::Packed, 0, 1, true)), (0, i, 1));
        }
        let rest = (LEN as u32 - PIECES) as i32;
        assert_eq!(
            data(pool.received(RingId::Packed, 0, rest, false)),
            (0, PIECES, rest as u32)
        );
        assert_eq!(pool.free(), 1, "full: the kernel moved on");
        for _ in 0..PIECES {
            pool.release(0);
            assert_eq!(pool.free(), 1, "freed while pieces are held");
        }
        pool.release(0);
        assert_eq!(pool.free(), 2);
    }

    #[test]
    #[should_panic(expected = "released more often than handed out")]
    fn a_buffer_released_more_often_than_handed_out_stops_the_worker() {
        let pool = pool(2, 64);
        assert!(pool.ensure_published(Some(RingId::Packed)));
        let a = data(pool.received(RingId::Packed, 0, 64, false));
        pool.release(a.0);
        pool.release(a.0);
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

/// Kani proofs (`scripts/check.sh kani`, docs/kani.md) for the receive
/// buffer pool.
///
/// Each harness starts from any pool state that [`BufPool::check`] accepts
/// ([`Sim::any`]) and applies one event, chosen freely from what the
/// drivers and the kernel can do: a model of the kernel takes a buffer from
/// a ring as io_uring does (the whole ring one buffer per receive; the
/// packed ring piece by piece, keeping a buffer at the head while room is
/// left), user space reads a piece it holds and perhaps releases it, the
/// io_uring driver refills or arms a receive, or the epoll driver takes a
/// spare buffer itself. The kernel model reads buffer ids from the ring
/// memory the pool wrote, through raw pointers, as the kernel does. A new
/// pool passes the check too, so the harnesses cover every sequence of
/// events, of any length.
///
/// After the event, for every buffer, the check proves:
///
/// * it is in exactly one place: a ring's published window, the spare
///   list, or held by user space; or, only as the packed ring's head
///   buffer, both in that ring and held (the kernel then writes only past
///   the pieces handed out, which the offset checks show);
/// * so the kernel can never take a buffer user space still reads, nor the
///   same buffer twice;
/// * no buffer is lost, and the pool's counts (`published`, `lent_to`,
///   `pieces`) agree with what the rings and user space really hold;
/// * a ring never holds more entries than it has slots (the kernel would
///   read an overwritten entry), and every entry names the buffer's real
///   address and length; the published tail is the pool's.
///
/// Kani also checks every raw pointer access on the way (ring entries, the
/// tail, buffer slices) for bounds and liveness.
///
/// Bounds: `COUNT` buffers of `BUF_LEN` bytes, one harness per driver
/// setup (epoll; io_uring with the whole ring; io_uring with both rings).
/// Not covered: more buffers; the driver code around the pool (which
/// completions reach `received`, the backpressure policy); kernel
/// behaviour beyond this model; concurrency with the kernel (each kernel
/// step is atomic between driver steps).
#[cfg(kani)]
mod proofs {
    use super::*;

    const COUNT: u16 = 2;
    const BUF_LEN: usize = 2;
    /// Most pieces user space can hold at once: each buffer is held by at
    /// most `BUF_LEN` one-byte pieces.
    const MAX_PIECES: usize = COUNT as usize * BUF_LEN;

    /// Every entry of ring `id` as the kernel reads them. Read at constant
    /// offsets and then picked by index: a read of ring memory at a
    /// symbolic offset costs Kani a term over the whole allocation, and a
    /// few of those exhaust its memory.
    fn entries(pool: &BufPool, id: RingId) -> [(u64, u32, u16); COUNT as usize] {
        let mut all = [(0, 0, 0); COUNT as usize];
        for (i, out) in all.iter_mut().enumerate() {
            // SAFETY: `i < count`, inside the ring allocation; the kernel
            // reads the entry fields like this.
            unsafe {
                let e = pool.ring(id).entries().add(i).cast::<RawEntry>();
                *out = (
                    ptr::addr_of!((*e).addr).read(),
                    ptr::addr_of!((*e).len).read(),
                    ptr::addr_of!((*e).bid).read(),
                );
            }
        }
        all
    }

    /// The entry the kernel finds at ring position `idx`.
    fn entry(pool: &BufPool, id: RingId, idx: u16) -> (u64, u32, u16) {
        entries(pool, id)[(idx & (pool.count - 1)) as usize]
    }

    /// The tail as the kernel reads it: the published copy.
    fn published_tail(pool: &BufPool, id: RingId) -> u16 {
        // SAFETY: the tail lives inside the ring allocation.
        unsafe {
            let tail = ptr::addr_of_mut!((*pool.ring(id).entries()).resv);
            AtomicU16::from_ptr(tail).load(Ordering::Acquire)
        }
    }

    struct Sim {
        /// On the stack, not in an `Rc`: Kani tracks the fields of a
        /// local precisely, but merges a heap object's fields across
        /// paths, and then every pointer read from it can point anywhere.
        pool: BufPool,
        uring: bool,
        packed: bool,
        /// Next entry the kernel takes, per ring.
        head: [u16; 2],
        /// Bytes the kernel has put into the packed ring's head buffer.
        filled: u32,
        /// Pieces user space holds: (buffer, offset, length).
        pieces: [Option<(u16, u32, u32)>; MAX_PIECES],
    }

    impl Sim {
        fn new(uring: bool, packed: bool) -> Self {
            // `BufPool::new` after its size checks, which build coded
            // errors: Kani would follow the code of every `dyn Error`.
            let ring = || Ring::with_map(Mapping::heap(Ring::size(COUNT)));
            let mem = Mapping::heap(COUNT as usize * BUF_LEN);
            let pool = BufPool::with_memory(COUNT, BUF_LEN, mem, [ring(), ring()]);
            if uring {
                pool.set_registered(RingId::Whole);
            }
            if packed {
                pool.set_registered(RingId::Packed);
            }
            let sim = Self {
                pool,
                uring,
                packed,
                head: [0; 2],
                filled: 0,
                pieces: [None; MAX_PIECES],
            };
            sim.check();
            sim
        }

        /// Any pool state [`check`](Self::check) accepts: every buffer
        /// spare, in a ring (in either order), or held in pieces; the
        /// packed ring's head buffer part-filled and perhaps held; any
        /// kernel head, targets and `ran_dry`. Built with the pool's own
        /// `push` and `publish_tail`, so the ring memory is what the pool
        /// writes.
        fn any(uring: bool, packed: bool) -> Self {
            let mut sim = Self::new(uring, packed);
            let pool = &sim.pool;
            pool.spare.borrow_mut().clear();
            for (r, id) in [RingId::Whole, RingId::Packed].into_iter().enumerate() {
                let head: u16 = kani::any();
                sim.head[r] = head;
                let ring = pool.ring(id);
                ring.tail.set(head);
                let registered = ring.registered.get();
                let target: usize = kani::any();
                kani::assume(target <= COUNT as usize && (registered || target == 0));
                ring.target.set(target);
                ring.ran_dry.set(registered && kani::any());
            }
            let first: u16 = if kani::any() { 1 } else { 0 };
            let mut held_only = [0u32; COUNT as usize];
            for n in 0..COUNT {
                let bid = (first + n) % COUNT;
                let home = if packed && kani::any() {
                    RingId::Packed
                } else {
                    RingId::Whole
                };
                pool.slots[bid as usize].home.set(home);
                match kani::any::<u8>() {
                    0 => pool.spare.borrow_mut().push(bid),
                    1 if uring => pool.push(RingId::Whole, bid),
                    2 if packed => pool.push(RingId::Packed, bid),
                    _ => {
                        let pieces: u32 = kani::any();
                        kani::assume(pieces >= 1 && pieces as usize <= BUF_LEN);
                        held_only[bid as usize] = pieces;
                    }
                }
            }
            pool.publish_tail(RingId::Whole);
            pool.publish_tail(RingId::Packed);
            // Pieces of buffers no ring holds: one byte each, or one
            // piece of any length.
            for bid in 0..COUNT {
                let pieces = held_only[bid as usize];
                sim.pool.slots[bid as usize].pieces.set(pieces);
                if pieces == 1 {
                    let len: u32 = kani::any();
                    kani::assume(len >= 1 && len as usize <= BUF_LEN);
                    sim.hold((bid, 0, len));
                } else {
                    for off in 0..pieces {
                        sim.hold((bid, off, 1));
                    }
                }
            }
            // The packed ring's head buffer: part-filled, and the filled
            // bytes perhaps held in one-byte pieces.
            if packed && sim.pool.ring(RingId::Packed).published.get() > 0 {
                let bid = entry(&sim.pool, RingId::Packed, sim.head[1]).2;
                let filled: u32 = kani::any();
                kani::assume((filled as usize) < BUF_LEN);
                let pieces: u32 = kani::any();
                kani::assume(pieces <= filled);
                let slot = &sim.pool.slots[bid as usize];
                slot.filled.set(filled);
                slot.pieces.set(pieces);
                sim.filled = filled;
                for off in 0..pieces {
                    sim.hold((bid, off, 1));
                }
            }
            sim.check();
            sim
        }

        fn hold(&mut self, piece: (u16, u32, u32)) {
            let free = self.pieces.iter_mut().find(|p| p.is_none());
            *free.expect("more pieces than bytes") = Some(piece);
        }

        /// The kernel completes a receive through ring `id`.
        fn kernel_receives(&mut self, id: RingId) {
            let pool = &self.pool;
            let head = self.head[id as usize];
            if head == published_tail(pool, id) {
                pool.ran_dry(id); // ENOBUFS
                return;
            }
            let bid = entry(pool, id, head).2;
            let room = match id {
                RingId::Whole => BUF_LEN as u32,
                RingId::Packed => BUF_LEN as u32 - self.filled,
            };
            let res: i32 = kani::any();
            kani::assume(res >= -1 && res <= room as i32);
            // The packed ring keeps a buffer while room is left in it.
            let more = id == RingId::Packed && res > 0 && (res as u32) < room;
            if id == RingId::Whole {
                self.head[0] = head.wrapping_add(1);
            }
            match pool.received(id, bid, res, more) {
                Received::Data { bid: b, off, len } => {
                    assert!(res > 0 && b == bid && len == res as u32);
                    if id == RingId::Packed {
                        assert!(off == self.filled, "a piece overlaps another");
                        self.filled += len;
                        if !more {
                            self.head[1] = head.wrapping_add(1);
                            self.filled = 0;
                        }
                    } else {
                        assert!(off == 0);
                    }
                    self.hold((b, off, len));
                }
                Received::Nothing => assert!(res <= 0),
            }
        }

        /// The epoll driver receives into a spare buffer.
        fn epoll_receives(&mut self) {
            if let Some((bid, ptr, len)) = self.pool.next_free() {
                assert!(len == BUF_LEN);
                // SAFETY: `next_free` lends `len` bytes at `ptr`.
                let _ = ptr; //XX
                self.pool.take_next();
                let n: u32 = kani::any();
                kani::assume(n >= 1 && n as usize <= BUF_LEN);
                self.hold((bid, 0, n));
            }
        }

        /// User space reads a piece it holds, then perhaps drops it.
        fn user_reads(&mut self) {
            let i: usize = kani::any();
            kani::assume(i < MAX_PIECES);
            if let Some((bid, off, len)) = self.pieces[i] {
                // What `copy_out` and `RecvBuf` read (a copy would add an
                // allocation of symbolic size).
                let bytes = self.pool.slice(bid, off, len);
                assert!(bytes.len() == len as usize);
                let _ = bytes[bytes.len() - 1];
                if kani::any() {
                    self.pool.release(bid);
                    self.pieces[i] = None;
                }
            }
        }

        fn step(&mut self) {
            match kani::any::<u8>() {
                // The io_uring driver arms a receive through a ring.
                0 if self.uring => {
                    let packed = self.packed && kani::any::<bool>();
                    let id = if packed {
                        RingId::Packed
                    } else {
                        RingId::Whole
                    };
                    self.pool.ensure_published(Some(id));
                }
                // The io_uring driver after a batch of completions.
                1 if self.uring => self.pool.refill(),
                2 if self.uring => self.kernel_receives(RingId::Whole),
                3 if self.packed => self.kernel_receives(RingId::Packed),
                4 if !self.uring => self.epoll_receives(),
                _ => self.user_reads(),
            }
            self.check();
        }

        fn check(&self) {
            self.pool.check(self.head, self.filled, &self.pieces);
        }
    }

    impl BufPool {
        fn check(&self, heads: [u16; 2], filled: u32, pieces: &[Option<(u16, u32, u32)>]) {
            // Plain loops: iterator adapters cost Kani far more code.
            let spare = self.spare.borrow();
            let mut windows = [0u16; 2];
            for r in 0..2 {
                let id = if r == 0 {
                    RingId::Whole
                } else {
                    RingId::Packed
                };
                let ring = self.ring(id);
                assert!(published_tail(self, id) == ring.tail.get());
                let window = ring.tail.get().wrapping_sub(heads[r]);
                assert!(window <= self.count, "a ring holds more entries than slots");
                assert!(window as usize == ring.published.get());
                windows[r] = window;
            }
            let mut in_spare = [0usize; COUNT as usize];
            let mut i = 0;
            while i < spare.len() {
                in_spare[spare[i] as usize] += 1;
                i += 1;
            }
            let mut in_ring = [[0usize; 2]; COUNT as usize];
            let base = self.mem.ptr.as_ptr() as u64;
            let mut packed_head = None;
            for r in 0..2 {
                let id = if r == 0 {
                    RingId::Whole
                } else {
                    RingId::Packed
                };
                let all = entries(self, id);
                let mut i = 0;
                while i < COUNT as usize {
                    // Position `i` holds a published entry if it is fewer
                    // than `window` places after the kernel's head.
                    let k = (i as u16).wrapping_sub(heads[r]) & (self.count - 1);
                    if k < windows[r] {
                        let (addr, len, b) = all[i];
                        assert!(b < self.count, "a ring entry names no buffer");
                        in_ring[b as usize][r] += 1;
                        assert!(addr == base + (b as usize * self.buf_len) as u64);
                        assert!(len as usize == self.buf_len);
                        if r == 1 && k == 0 {
                            packed_head = Some(b as usize);
                        }
                    }
                    i += 1;
                }
            }
            let mut held = [0usize; COUNT as usize];
            let mut i = 0;
            while i < pieces.len() {
                if let Some((b, _, _)) = pieces[i] {
                    held[b as usize] += 1;
                }
                i += 1;
            }
            let mut bid = 0;
            while bid < COUNT as usize {
                let slot = &self.slots[bid];
                let rings = in_ring[bid][0] + in_ring[bid][1];
                assert!(rings <= 1, "a buffer is in the rings twice");
                assert!(
                    in_spare[bid] + rings <= 1,
                    "a buffer is spare twice or also in a ring"
                );
                match slot.lent_to.get() {
                    None => assert!(rings == 0, "lent_to misses a ring"),
                    Some(id) => {
                        assert!(in_ring[bid][id as usize] == 1, "lent_to names a wrong ring")
                    }
                }
                assert!(
                    slot.pieces.get() as usize == held[bid],
                    "the pool miscounts pieces"
                );
                if held[bid] > 0 {
                    assert!(in_spare[bid] == 0, "a held buffer is spare");
                    if rings > 0 {
                        // Only the packed ring's head buffer: the kernel
                        // writes into it past `filled`, never over a piece.
                        assert!(in_ring[bid][1] == 1);
                        assert!(packed_head == Some(bid));
                        assert!(slot.filled.get() == filled);
                    }
                }
                assert!(in_spare[bid] + rings + held[bid] > 0, "a buffer was lost");
                bid += 1;
            }
            assert!(self.free() == spare.len() + windows[0] as usize + windows[1] as usize);
        }
    }

    /// One event from any valid state: with [`Sim::new`] valid too, this
    /// covers every sequence of events, of any length.
    fn one_event(uring: bool, packed: bool) {
        let mut sim = Sim::any(uring, packed);
        sim.step();
        // The harness's bookkeeping; dropping the pool only adds drop code.
        std::mem::forget(sim);
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn epoll_buffers_are_never_shared_or_lost() {
        Sim::new(false, false);
        one_event(false, false);
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn whole_ring_buffers_are_never_shared_or_lost() {
        Sim::new(true, false);
        one_event(true, false);
    }

    #[kani::proof]
    #[kani::unwind(5)]
    fn packed_ring_buffers_are_never_shared_or_lost() {
        Sim::new(true, true);
        one_event(true, true);
    }
}
