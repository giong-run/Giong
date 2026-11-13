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

/// The per-worker pool of receive buffers shared with the kernel through an
/// io_uring *buffer ring* (`IORING_REGISTER_PBUF_RING`).
///
/// The kernel takes a buffer from the ring whenever a multishot receive has
/// data and reports its id in the completion; the buffer then belongs to user
/// space until [`BufPool::recycle`] puts it back.
/// An anonymous, private, zero-filled memory mapping.
///
/// Page aligned (as the buffer ring must be) and lazily backed: pages only
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

pub(crate) struct BufPool {
    /// `count` ring entries shared with the kernel.
    ring: Mapping,
    /// `count * buf_len` bytes of buffer space.
    mem: Mapping,
    count: u16,
    buf_len: usize,
    /// Our private copy of the ring tail (the published copy lives in `ring`).
    tail: Cell<u16>,
    /// Buffers in the ring, available to the kernel.
    published: Cell<usize>,
    /// How many free buffers to keep in the ring: 0 with the epoll driver,
    /// which takes buffers from `spare` itself.
    target: Cell<usize>,
    /// A receive found the ring empty since the last refill.
    ran_dry: Cell<bool>,
    /// Free buffers that are not in the ring, the most recently used on top.
    ///
    /// The kernel takes ring entries in order and every buffer given back
    /// goes to the end of the ring, so a ring holding all `count` buffers
    /// cycles through every one of them and makes all of their memory
    /// resident, however few are in use at a time. Keeping only `target` in
    /// the ring and the rest here, reused last-in first-out, keeps the
    /// memory a worker touches close to what its traffic needs.
    spare: RefCell<Vec<u16>>,
    /// Tasks waiting for a buffer to come back.
    waiters: RefCell<Vec<Waker>>,
}

/// Buffers an io_uring worker keeps in its ring at first. The number doubles,
/// up to the pool's size, whenever a batch of completions takes most of them
/// or the kernel finds the ring empty.
const FIRST_TARGET: usize = 16;

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
        // One page more than the entries need: Linux 6.0 (and 5.19) sizes the
        // ring as `struct_size(br, bufs, count)`, 16 bytes more than `count`
        // entries, so for 256 entries or more it pins one page past the end
        // and fails with EFAULT unless that page happens to be mapped.
        // Fixed in 6.1 ("io_uring: fix size calculation when registering buf
        // ring"); found by running the tests on a real 6.0 kernel.
        let ring_len = count as usize * std::mem::size_of::<BufRingEntry>() + page_size();
        let ring = Mapping::new(ring_len)?;
        let mem_size = (count as usize).checked_mul(buf_len).ok_or_else(|| {
            raise(
                Code::InvalidConfig,
                format!("{count} recv buffers of {buf_len} bytes do not fit in memory"),
                "pass fewer or smaller buffers to Builder::recv_buffers.",
            )
        })?;
        let mem = Mapping::new(mem_size)?;

        let pool = Self {
            ring,
            mem,
            count,
            buf_len,
            tail: Cell::new(0),
            published: Cell::new(0),
            target: Cell::new(0),
            ran_dry: Cell::new(false),
            // Buffer 0 on top: the lowest addresses are used first.
            spare: RefCell::new((0..count).rev().collect()),
            waiters: RefCell::new(Vec::new()),
        };
        Ok(Rc::new(pool))
    }

    /// The io_uring driver registered the ring: start filling it.
    pub(crate) fn start_ring(&self) {
        let target = FIRST_TARGET.min(self.count as usize);
        self.target.set(target);
        self.fill_to(target);
    }

    /// Tops the ring up to its target from the spare buffers once it is
    /// half empty. The io_uring driver calls it after every batch of
    /// completions, before the kernel can take more. If the kernel found the
    /// ring empty during the batch, or the batch took three quarters of it,
    /// the target doubles first (once per batch, however many receives ran
    /// dry).
    pub(crate) fn refill(&self) {
        let target = self.target.get();
        if target == 0 {
            return;
        }
        let ran_dry = self.ran_dry.replace(false);
        let published = self.published.get();
        if ran_dry || published * 4 < target {
            self.target.set((target * 2).min(self.count as usize));
        } else if published * 2 >= target {
            // Plenty left: the buffers this batch took go straight back into
            // the ring when their readers drop them.
            return;
        }
        self.fill_to(self.target.get());
    }

    /// The kernel found the ring empty: the next [`refill`](Self::refill)
    /// keeps more buffers in it.
    pub(crate) fn ran_dry(&self) {
        self.ran_dry.set(true);
    }

    fn fill_to(&self, target: usize) {
        let mut spare = self.spare.borrow_mut();
        let before = self.published.get();
        while self.published.get() < target {
            let Some(bid) = spare.pop() else { break };
            self.push(bid);
            self.published.set(self.published.get() + 1);
        }
        drop(spare);
        if self.published.get() != before {
            self.publish_tail();
        }
    }

    pub(crate) fn ring_addr(&self) -> u64 {
        self.ring.ptr.as_ptr() as u64
    }

    fn ring_ptr(&self) -> *mut BufRingEntry {
        self.ring.ptr.as_ptr() as *mut BufRingEntry
    }

    pub(crate) fn count(&self) -> u16 {
        self.count
    }

    /// Buffers not in use: in the ring or spare.
    pub(crate) fn free(&self) -> usize {
        self.published.get() + self.spare.borrow().len()
    }

    /// Makes sure the ring holds a buffer before a receive is armed, if any
    /// buffer is free. Returns `false` if none is.
    pub(crate) fn ensure_published(&self) -> bool {
        if self.target.get() != 0 && self.published.get() == 0 {
            self.fill_to(1);
        }
        self.free() > 0
    }

    /// `true` if every free buffer is in the ring.
    pub(crate) fn no_spare(&self) -> bool {
        self.spare.borrow().is_empty()
    }

    /// Records that the kernel took a buffer from the ring.
    pub(crate) fn taken(&self) {
        self.published.set(self.published.get().saturating_sub(1));
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
    /// now holds data and belongs to user space until it is recycled.
    pub(crate) fn take_next(&self) {
        let taken = self.spare.borrow_mut().pop();
        debug_assert!(taken.is_some());
    }

    /// Gives a buffer back (to the ring if it is below its target, else to
    /// the spare buffers) and wakes tasks starved for buffers.
    pub(crate) fn recycle(&self, bid: u16) {
        debug_assert!(bid < self.count);
        if self.published.get() < self.target.get() {
            self.push(bid);
            self.publish_tail();
            self.published.set(self.published.get() + 1);
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

    fn push(&self, bid: u16) {
        let tail = self.tail.get();
        let idx = (tail & (self.count - 1)) as usize;
        // SAFETY: `idx < count`, the ring allocation holds `count` entries, and the
        // kernel only reads entries below the published tail. The setters do not
        // touch the `resv` field, which doubles as the ring tail in entry 0.
        unsafe {
            let entry = &mut *self.ring_ptr().add(idx);
            entry.set_addr(self.mem.ptr.as_ptr().add(bid as usize * self.buf_len) as u64);
            entry.set_len(self.buf_len as u32);
            entry.set_bid(bid);
        }
        self.tail.set(tail.wrapping_add(1));
    }

    fn publish_tail(&self) {
        // SAFETY: the tail lives inside our ring allocation and is only ever
        // accessed atomically (the kernel reads it concurrently).
        unsafe {
            let tail = BufRingEntry::tail(self.ring_ptr()) as *mut u16;
            AtomicU16::from_ptr(tail).store(self.tail.get(), Ordering::Release);
        }
    }

    /// Copies a buffer's contents to the heap (used to free pool buffers that
    /// nobody is reading from).
    pub(crate) fn copy_out(&self, bid: u16, len: usize) -> Vec<u8> {
        self.slice(bid, len).to_vec()
    }

    fn slice(&self, bid: u16, len: usize) -> &[u8] {
        assert!(
            bid < self.count && len <= self.buf_len,
            "kernel returned an invalid buffer"
        );
        // SAFETY: in bounds per the assert; the kernel initialised `len` bytes and
        // does not touch the buffer again until it is recycled.
        unsafe {
            std::slice::from_raw_parts(self.mem.ptr.as_ptr().add(bid as usize * self.buf_len), len)
        }
    }
}

// The mappings are released when the pool is dropped. The runtime only lets
// the last reference go once the ring that used this memory is quiesced.

/// Bytes received by [`TcpStream::recv`](crate::net::TcpStream::recv).
///
/// The data normally lives in a buffer borrowed from the worker's
/// kernel-shared pool; dropping the `RecvBuf` returns the buffer to the pool.
/// Copy out what you need and drop it promptly — do not hold one across an
/// `.await`, or slow peers can starve the pool.
///
/// An empty `RecvBuf` means the peer closed the connection.
pub struct RecvBuf {
    inner: Inner,
}

enum Inner {
    /// A pool buffer: `(pool, buffer id, length)`.
    Pooled(Rc<BufPool>, u16, usize),
    /// Data the driver moved to the heap because the pool was under pressure.
    Heap(Vec<u8>),
    Eof,
}

impl RecvBuf {
    pub(crate) fn pooled(pool: Rc<BufPool>, bid: u16, len: usize) -> Self {
        Self {
            inner: Inner::Pooled(pool, bid, len),
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
            Inner::Pooled(pool, bid, len) => pool.slice(*bid, *len),
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
        if let Inner::Pooled(pool, bid, _) = &self.inner {
            pool.recycle(*bid);
        }
    }
}
