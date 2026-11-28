//! Verus proof of `src/ipc/ring.rs`: for every capacity, every sequence of
//! calls and every value the peer process writes into the shared memory, the
//! ring never reads or writes outside its data area, never panics (no
//! arithmetic overflow, no division by zero, no failed `debug_assert!`), and
//! every loop ends. See docs/verus.md.
//!
//! The code between the `verus-mirror` markers is `src/ipc/ring.rs` itself:
//! `scripts/verus.py drift` strips the proof annotations from it and fails
//! unless what remains is token for token the same as the source file without
//! its tests. Everything outside the markers is the specification and the
//! trusted model of the few things Verus cannot see into (raw pointers).
//!
//! Every value read from shared memory is unconstrained: vstd specifies the
//! standard atomics with no knowledge of what a load returns, and the record
//! heads are read through `u32::from_le_bytes` of bytes the proof knows
//! nothing about. That is the untrusted peer.

#![allow(unused_imports, dead_code)]

use vstd::prelude::*;

verus! {
global size_of usize == 8;
} // verus!

verus! {

// ---------------------------------------------------------------------------
// Trusted model of raw memory (not proven; each line is a fact about Rust's
// pointers or about the mapping that `Ring::new`'s safety contract states).
// ---------------------------------------------------------------------------

} // verus!

pub mod mem {
    use vstd::prelude::*;

    verus! {

    /// `n` elements from `p` lie inside one live allocation that this process may
    /// write (and read).
    pub uninterp spec fn writable<T>(p: *mut T, n: int) -> bool;

    /// `n` elements from `p` may be read.
    pub uninterp spec fn readable<T>(p: *const T, n: int) -> bool;

    /// `p` moved forward by `k` elements.
    pub uninterp spec fn offset<T>(p: *mut T, k: int) -> *mut T;

    /// Inside a writable block of `n` elements, the `m` elements `k` in are
    /// writable and readable, and so is its front part.
    pub broadcast axiom fn axiom_inside<T>(p: *mut T, n: int, k: int, m: int)
        requires
            writable(p, n),
            0 <= k,
            0 <= m,
            k + m <= n,
        ensures
            #![trigger writable(p, n), writable(offset(p, k), m)]
            #![trigger writable(p, n), readable(offset(p, k) as *const T, m)]
            writable(offset(p, k), m),
            readable(offset(p, k) as *const T, m),
    ;

    pub broadcast axiom fn axiom_front<T>(p: *mut T, n: int, m: int)
        requires
            writable(p, n),
            0 <= m <= n,
        ensures
            #![trigger writable(p, n), writable(p, m)]
            writable(p, m),
    ;

    /// `pointer::add` is defined only within one allocation (one past its end
    /// included).
    pub assume_specification<T>[ <*mut T>::add ](p: *mut T, count: usize) -> (r: *mut T)
        requires
            writable(p, count as int),
        ensures
            r == offset(p, count as int),
    ;

    /// Both ranges must be valid; they never overlap here: the source is a local
    /// array or the caller's message, the destination the shared mapping (or a
    /// local array, from the mapping).
    pub assume_specification<T>[ std::ptr::copy_nonoverlapping::<T> ](
        src: *const T,
        dst: *mut T,
        count: usize,
    )
        requires
            readable(src, count as int),
            writable(dst, count as int),
    ;

    pub assume_specification<T>[ <[T]>::as_ptr ](s: &[T]) -> (r: *const T)
        ensures
            readable(r, s@.len() as int),
    ;

    pub assume_specification<T>[ <[T]>::as_mut_ptr ](s: &mut [T]) -> (r: *mut T)
        ensures
            writable(r, old(s)@.len() as int),
    ;

    /// The bytes of a record head's length: their values do not matter here.
    pub assume_specification[ <u32>::to_le_bytes ](x: u32) -> [u8; 4];

    /// The mapping `Ring::new` is given: `len` bytes at `base`, laid out as the
    /// module docs say.
    pub uninterp spec fn mapping(base: *mut u8, len: int) -> bool;

    } // verus!
}

use mem::*;

verus! {

broadcast use {mem::axiom_inside, mem::axiom_front};

// ---------------------------------------------------------------------------
// The specification.
// ---------------------------------------------------------------------------

/// What both sides of a ring keep true between calls.
spec fn ring_wf(r: &Ring) -> bool {
    &&& MIN_CAPACITY <= r.capacity <= MAX_CAPACITY
    &&& r.capacity % 8 == 0
    &&& writable(r.data, r.capacity as int)
    &&& r.pos % 8 == 0
    &&& r.seen % 8 == 0
}

/// A producer: `seen` is a head it checked, at most a ring behind its tail.
spec fn producer_wf(r: &Ring) -> bool {
    &&& ring_wf(r)
    &&& r.seen <= r.pos
    &&& r.pos - r.seen <= r.capacity
}

/// A consumer: `seen` is a tail it checked, at most a ring ahead of its head.
spec fn consumer_wf(r: &Ring) -> bool {
    &&& ring_wf(r)
    &&& r.pos <= r.seen
    &&& r.seen - r.pos <= r.capacity
}

/// The offset of a position in the data area is aligned and leaves room for a
/// record head; skipping to the end of the area lands on offset 0.
proof fn lemma_offset(pos: u64, cap: u64)
    requires
        pos % 8 == 0,
        cap % 8 == 0,
        cap > 0,
    ensures
        (pos % cap) % 8 == 0,
        pos % cap + 8 <= cap,
        (pos as int + (cap - pos % cap)) % (cap as int) == 0,
{
    let p = pos as int;
    let c = cap as int;
    let q = p / c;
    let r = p % c;
    vstd::arithmetic::div_mod::lemma_fundamental_div_mod(p, c);
    let k = c / 8;
    assert(c == 8 * k);
    assert(p == 8 * (q * k) + r) by (nonlinear_arith)
        requires
            p == c * q + r,
            c == 8 * k,
    ;
    vstd::arithmetic::div_mod::lemma_mod_multiples_vanish(q * k, r, 8);
    vstd::arithmetic::div_mod::lemma_mod_multiples_vanish(q * k, r, 8);
    assert(r % 8 == 0);
    assert(p + (c - r) == c * (q + 1)) by (nonlinear_arith)
        requires
            p == c * q + r,
    ;
    vstd::arithmetic::div_mod::lemma_mod_multiples_basic(q + 1, c);
}

/// The value of `MAX_CAPACITY`, which Verus does not compute from `1 << 30`.
proof fn lemma_max_capacity()
    ensures
        MAX_CAPACITY == 0x4000_0000,
{
    assert((1usize << 30usize) == 0x4000_0000usize) by (bit_vector);
}

/// `padded` rounds up to a multiple of 8, by at most 7.
proof fn lemma_padded(len: usize)
    requires
        len <= 0xffff_ffff_ffff_fff0usize,
    ensures
        ((add(len, 7)) & !7usize) % 8 == 0,
        len <= ((add(len, 7)) & !7usize),
        ((add(len, 7)) & !7usize) <= len + 7,
{
    assert(((add(len, 7)) & !7usize) % 8 == 0 && len <= ((add(len, 7)) & !7usize) && ((add(
        len,
        7,
    )) & !7usize) <= add(len, 7)) by (bit_vector)
        requires
            len <= 0xffff_ffff_ffff_fff0usize,
    ;
}

// verus-mirror: begin

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// `"GIONGIPC"` as a little-endian u64.
#[verifier::external_body]
pub const MAGIC: u64 = u64::from_le_bytes(*b"GIONGIPC");
pub const VERSION: u32 = 1;
/// Offset of the first data area.
pub const DATA_OFFSET: usize = 4096;
/// The smallest and largest capacity of one direction.
pub const MIN_CAPACITY: usize = 4096;
pub const MAX_CAPACITY: usize = 1 << 30;
/// Record head length of a wrap marker.
const SKIP: u32 = u32::MAX;
const HEAD: usize = 8;

/// `consumer_sleeping` and `producer_sleeping` values: awake (or watched
/// by its spinning worker: nothing to do), and asleep in the kernel (wake
/// it). A third value from a peer counts as awake.
pub const AWAKE: u32 = 0;
pub const ASLEEP: u32 = 1;

/// The producer has dropped its half: no more records come.
pub const PRODUCER_CLOSED: u32 = 1;
/// The consumer has dropped its half: records sent now are never read.
pub const CONSUMER_CLOSED: u32 = 2;

#[repr(C, align(64))]
pub(crate) struct Line<T>(pub(crate) T);

#[repr(C, align(64))]
pub(crate) struct Header {
    pub(crate) magic: AtomicU64,
    pub(crate) version: AtomicU32,
    pub(crate) capacity: AtomicU32,
}

/// The shared state of one direction.
#[repr(C)]
pub(crate) struct Dir {
    pub(crate) tail: Line<AtomicU64>,
    pub(crate) head: Line<AtomicU64>,
    /// [`ASLEEP`] while the consumer is about to sleep or sleeps; the
    /// producer swaps it to [`AWAKE`] and, if it was asleep, wakes it. It
    /// stays awake while the consumer's worker busy-polls the ring.
    pub(crate) consumer_sleeping: Line<AtomicU32>,
    /// The same for a producer waiting for room.
    pub(crate) producer_sleeping: Line<AtomicU32>,
    /// [`PRODUCER_CLOSED`] and [`CONSUMER_CLOSED`].
    pub(crate) flags: Line<AtomicU32>,
}

#[repr(C)]
pub(crate) struct Control {
    pub(crate) header: Header,
    pub(crate) dirs: [Dir; 2],
}

#[verifier::external]
const _: () = assert!(std::mem::size_of::<Control>() <= DATA_OFFSET);

/// Bytes the mapping of a channel with this capacity takes.
pub(crate) fn mapping_len(capacity: usize) -> usize
    requires
        capacity <= MAX_CAPACITY,
{
    proof {
        lemma_max_capacity();
    }
    DATA_OFFSET + 2 * capacity
}

/// The largest message a channel of this capacity carries: a quarter of a
/// direction, so that a record always fits even after a wrap.
pub(crate) fn max_message(capacity: usize) -> (r: usize)
    requires
        MIN_CAPACITY <= capacity <= MAX_CAPACITY,
    ensures
        r == capacity / 4 - HEAD,
{
    capacity / 4 - HEAD
}

fn padded(len: usize) -> (r: usize)
    requires
        len <= 0xffff_ffff_ffff_fff0usize,
    ensures
        r % 8 == 0,
        len <= r <= len + 7,
{
    proof {
        lemma_padded(len);
    }
    (len + 7) & !7
}

/// The peer broke the layout's rules.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Corrupt(pub(crate) &'static str);

/// One direction as seen by its producer or consumer: the shared state, the
/// data area, and this side's own position.
pub(crate) struct Ring {
    dir: *const Dir,
    data: *mut u8,
    capacity: u64,
    /// The tail (producer) or head (consumer) this side has published.
    pos: u64,
    /// The other side's position as last read (and checked): a producer
    /// reads `head` again only when this says the ring is full, a consumer
    /// reads `tail` again only when this says it is empty, so that the
    /// cache line the other side writes is not pulled over per message.
    seen: u64,
}

/// What [`Ring::push`] did.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Push {
    Done,
    Full,
}

impl Ring {
    /// # Safety
    ///
    /// `base` points at a live mapping of [`mapping_len`]`(capacity)` bytes
    /// laid out as described above, which outlives the `Ring`; `capacity`
    /// is a power of two between [`MIN_CAPACITY`] and [`MAX_CAPACITY`];
    /// `dir` is 0 or 1; at most one producer and one consumer `Ring` per
    /// direction exist across both processes (the peer may break this; the
    /// checks below catch what it can do to us).
    #[verifier::external_body]
    pub(crate) unsafe fn new(base: *mut u8, capacity: usize, dir: usize) -> (r: Self)
        requires
            mapping(base, DATA_OFFSET + 2 * capacity),
            MIN_CAPACITY <= capacity <= MAX_CAPACITY,
            capacity % 8 == 0,
            dir < 2,
        ensures
            producer_wf(&r),
            consumer_wf(&r),
            r.capacity == capacity,
            r.pos == 0,
    {
        let control = base.cast::<Control>();
        // SAFETY: in bounds of the mapping, per the caller.
        unsafe {
            Self {
                dir: std::ptr::addr_of!((*control).dirs[dir]),
                data: base.add(DATA_OFFSET + dir * capacity),
                capacity: capacity as u64,
                pos: 0,
                seen: 0,
            }
        }
    }

    /// This side's own published position.
    pub(crate) fn pos(&self) -> (r: u64)
        ensures
            r == self.pos,
    {
        self.pos
    }

    pub(crate) fn dir_ptr(&self) -> *const Dir {
        self.dir
    }

    /// Where the bytes of the first message sent on a fresh ring lie, for
    /// tests that play a peer writing into the ring.
    #[cfg(test)]
    pub(crate) fn first_message_for_test(&self) -> *mut u8 {
        // SAFETY: the data area is longer than a record header.
        unsafe { self.data.add(HEAD) }
    }

    #[verifier::external_body]
    pub(crate) fn dir(&self) -> &Dir {
        // SAFETY: valid while the mapping lives (see `new`); every field is
        // an atomic, so the peer writing it at the same time is fine.
        unsafe { &*self.dir }
    }

    /// The consumer's published head, checked against this producer's tail.
    fn checked_head(&self) -> (r: Result<u64, Corrupt>)
        requires
            ring_wf(self),
        ensures
            r matches Ok(head) ==> head <= self.pos && self.pos - head <= self.capacity && head
                % 8 == 0,
    {
        let head = self.dir().head.0.load(Ordering::SeqCst);
        let used = self.pos.wrapping_sub(head);
        if head > self.pos || used > self.capacity || used % 8 != 0 {
            return Err(Corrupt(
                "the receiving process moved its read position out of range",
            ));
        }
        Ok(head)
    }

    /// The producer's published tail, checked against this consumer's head.
    fn checked_tail(&self) -> (r: Result<u64, Corrupt>)
        requires
            ring_wf(self),
        ensures
            r matches Ok(tail) ==> self.pos <= tail && tail - self.pos <= self.capacity && tail
                % 8 == 0,
    {
        let tail = self.dir().tail.0.load(Ordering::SeqCst);
        let avail = tail.wrapping_sub(self.pos);
        if tail < self.pos || avail > self.capacity || avail % 8 != 0 {
            return Err(Corrupt(
                "the sending process moved its write position out of range",
            ));
        }
        Ok(tail)
    }

    fn write_head(&mut self, off: u64, len: u32)
        requires
            ring_wf(old(self)),
            off % 8 == 0,
            off + 8 <= old(self).capacity,
        ensures
            *final(self) == *old(self),
    {
        let mut head = [0u8; HEAD];
        head[..4].copy_from_slice(&len.to_le_bytes());
        // SAFETY: `off + 8 <= capacity` (offsets are multiples of 8 and the
        // capacity is too), inside this direction's data area, in space the
        // consumer has released.
        unsafe { std::ptr::copy_nonoverlapping(head.as_ptr(), self.data.add(off as usize), HEAD) };
    }

    /// Producer: appends one message, or says the ring has no room for it.
    /// `msg.len()` must be at most [`max_message`].
    #[verifier::rlimit(100)]
    pub(crate) fn push(&mut self, msg: &[u8]) -> (r: Result<Push, Corrupt>)
        requires
            producer_wf(old(self)),
            msg@.len() <= old(self).capacity / 4 - HEAD,
            // The producer has sent fewer than 2^64 - 2^30 bytes.
            old(self).pos <= u64::MAX - old(self).capacity,
        ensures
            producer_wf(final(self)),
            final(self).capacity == old(self).capacity,
            r matches Ok(Push::Done) ==> old(self).pos < final(self).pos && final(self).pos <= final(self).seen
                + final(self).capacity,
            !(r matches Ok(Push::Done)) ==> final(self).pos == old(self).pos,
    {
        debug_assert!(msg.len() <= max_message(self.capacity as usize));
        let total = (HEAD + padded(msg.len())) as u64;
        let off = self.pos % self.capacity;
        proof {
            lemma_offset(self.pos, self.capacity);
        }
        let contiguous = self.capacity - off;
        let need = if total <= contiguous {
            total
        } else {
            contiguous + total
        };
        if need > self.capacity - (self.pos - self.seen) {
            self.seen = self.checked_head()?;
            if need > self.capacity - (self.pos - self.seen) {
                return Ok(Push::Full);
            }
        }
        let mut pos = self.pos;
        let mut off = off;
        if total > contiguous {
            self.write_head(off, SKIP);
            pos += contiguous;
            off = 0;
        }
        self.write_head(off, msg.len() as u32);
        // SAFETY: `off + total <= capacity`, in released space (checked above).
        unsafe {
            std::ptr::copy_nonoverlapping(
                msg.as_ptr(),
                self.data.add(off as usize + HEAD),
                msg.len(),
            );
        }
        pos += total;
        self.pos = pos;
        // Not visible to the consumer until `tail` is published (see
        // `Pending` in ipc.rs), so that a burst of records costs one store
        // to the line the consumer polls.
        Ok(Push::Done)
    }

    /// Producer: makes every pushed record visible (tests; the channel
    /// publishes through `Pending`).
    #[cfg(test)]
    pub(crate) fn publish(&self) {
        self.dir().tail.0.store(self.pos, Ordering::SeqCst);
    }

    /// Producer: whether a message of `len` bytes would fit now.
    pub(crate) fn has_room(&mut self, len: usize) -> (r: Result<bool, Corrupt>)
        requires
            producer_wf(old(self)),
            len <= old(self).capacity / 4 - HEAD,
        ensures
            producer_wf(final(self)),
            final(self).pos == old(self).pos,
            final(self).capacity == old(self).capacity,
    {
        let head = self.checked_head()?;
        self.seen = head;
        let free = self.capacity - (self.pos - head);
        let total = (HEAD + padded(len)) as u64;
        proof {
            lemma_offset(self.pos, self.capacity);
        }
        let contiguous = self.capacity - self.pos % self.capacity;
        let need = if total <= contiguous {
            total
        } else {
            contiguous + total
        };
        Ok(need <= free)
    }

    /// Consumer: the next message as (pointer, length, record length), past
    /// any wrap marker, or `None` when the ring is empty. The record stays
    /// in the ring until [`release`](Self::release).
    pub(crate) fn peek(&mut self) -> (r: Result<Option<(*const u8, usize, u64)>, Corrupt>)
        requires
            consumer_wf(old(self)),
        ensures
            consumer_wf(final(self)),
            final(self).capacity == old(self).capacity,
            old(self).pos <= final(self).pos,
            r matches Ok(Some((ptr, len, total))) ==> {
                &&& len <= final(self).capacity / 4 - HEAD
                &&& total == HEAD + len + (total - HEAD - len)
                &&& total % 8 == 0
                &&& HEAD + len <= total
                &&& final(self).pos + total <= final(self).seen
                &&& final(self).pos % final(self).capacity + total <= final(self).capacity
                &&& readable(ptr, len as int)
            },
    {
        loop
            invariant
                consumer_wf(self),
                self.capacity == old(self).capacity,
                old(self).pos <= self.pos,
            decreases
                (if self.pos % self.capacity == 0 {
                    0int
                } else {
                    1int
                }),
        {
            if self.seen == self.pos {
                self.seen = self.checked_tail()?;
            }
            let avail = self.seen - self.pos;
            if avail == 0 {
                return Ok(None);
            }
            let off = self.pos % self.capacity;
            proof {
                lemma_offset(self.pos, self.capacity);
            }
            let mut head = [0u8; 4];
            // SAFETY: `off + 8 <= capacity`, inside the data area. The bytes
            // may change under us if the peer misbehaves; they are copied
            // out once and checked.
            unsafe {
                std::ptr::copy_nonoverlapping(self.data.add(off as usize), head.as_mut_ptr(), 4)
            };
            let len = u32::from_le_bytes(head);
            let contiguous = self.capacity - off;
            if len == SKIP {
                // A producer writes a marker only before a record that does
                // not fit in what is left of the area, and every record fits
                // from offset 0: a marker there comes from a broken peer, and
                // following it would let the peer keep this loop, and the
                // worker, turning for as long as it keeps moving `tail`.
                if off == 0 {
                    return Err(Corrupt("a wrap marker at the start of the data area"));
                }
                if contiguous > avail {
                    return Err(Corrupt("a wrap marker points past the written data"));
                }
                self.pos += contiguous;
                self.dir().head.0.store(self.pos, Ordering::Release);
                continue;
            }
            let len = len as usize;
            let total = (HEAD + padded(len)) as u64;
            if len > max_message(self.capacity as usize) || total > avail || total > contiguous {
                return Err(Corrupt("a message length points past the written data"));
            }
            // SAFETY: `off + 8 + len <= capacity`.
            let ptr = unsafe { self.data.add(off as usize + HEAD) };
            return Ok(Some((ptr, len, total)));
        }
    }

    /// Consumer: gives the record [`peek`](Self::peek) returned back to the
    /// producer.
    pub(crate) fn release(&mut self, total: u64)
        requires
            consumer_wf(old(self)),
            total % 8 == 0,
            old(self).pos + total <= old(self).seen,
        ensures
            consumer_wf(final(self)),
            final(self).capacity == old(self).capacity,
    {
        self.pos += total;
        // Sequentially consistent for the same reason as `tail` in `push`.
        self.dir().head.0.store(self.pos, Ordering::SeqCst);
    }

    /// Consumer: a cheap look at whether the producer moved `tail`, for
    /// polling in a loop (unchecked: `peek` checks what it reads).
    pub(crate) fn tail_moved(&self) -> bool {
        self.dir().tail.0.load(Ordering::Relaxed) != self.pos
    }

    /// Consumer: whether a record is waiting (corrupt state counts as
    /// waiting, so that the next `peek` reports it).
    pub(crate) fn has_data(&mut self) -> (r: bool)
        requires
            consumer_wf(old(self)),
        ensures
            consumer_wf(final(self)),
            final(self).pos == old(self).pos,
            final(self).capacity == old(self).capacity,
    {
        match self.checked_tail() {
            Ok(tail) => {
                self.seen = tail;
                tail != self.pos
            }
            Err(_) => true,
        }
    }
}
// verus-mirror: end

} // verus!
