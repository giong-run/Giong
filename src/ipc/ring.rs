//! The memory two processes share for one channel, and the byte rings in
//! it. No system calls here: the layout, the records, and the checks on
//! everything read from memory the peer can write.
//!
//! Layout (version 1), all offsets fixed:
//!
//! ```text
//! 0      Header: magic, version, capacity
//! 64     Dir 0 (connector to acceptor): five 64-byte lines
//! 384    Dir 1 (acceptor to connector): five 64-byte lines
//! 4096   data of dir 0: `capacity` bytes
//! 4096 + capacity   data of dir 1: `capacity` bytes
//! ```
//!
//! A record is an 8-byte head (the message length as a little-endian u32,
//! then 4 unused bytes) and the message, padded to 8 bytes. A record never
//! wraps: when it does not fit before the end, the producer writes a head
//! with length [`SKIP`] and starts again at offset 0. Positions only grow
//! (u64), and the offset of a position is `pos % capacity`.
//!
//! The producer of a direction is the only writer of its `tail`, the
//! consumer the only writer of its `head`. Each side keeps its own position
//! locally and never trusts the copy in shared memory; the other side's
//! position is checked before use, so a peer that writes garbage makes the
//! channel fail with `GIONG-E0091` and never makes this process read or
//! write outside the mapping.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// `"GIONGIPC"` as a little-endian u64.
pub(crate) const MAGIC: u64 = u64::from_le_bytes(*b"GIONGIPC");
pub(crate) const VERSION: u32 = 1;
/// Offset of the first data area.
pub(crate) const DATA_OFFSET: usize = 4096;
/// The smallest and largest capacity of one direction.
pub(crate) const MIN_CAPACITY: usize = 4096;
pub(crate) const MAX_CAPACITY: usize = 1 << 30;
/// Record head length of a wrap marker.
const SKIP: u32 = u32::MAX;
const HEAD: usize = 8;

/// `consumer_sleeping` values: awake, asleep in the kernel (wake it), and
/// watched by its spinning worker (nothing to do).
pub(crate) const AWAKE: u32 = 0;
pub(crate) const ASLEEP: u32 = 1;
pub(crate) const SPINNING: u32 = 2;

/// The producer has dropped its half: no more records come.
pub(crate) const PRODUCER_CLOSED: u32 = 1;
/// The consumer has dropped its half: records sent now are never read.
pub(crate) const CONSUMER_CLOSED: u32 = 2;

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
    /// 1 while the consumer is about to sleep or sleeps; the producer swaps
    /// it to 0 and, if it was 1, wakes it. [`SPINNING`] while the consumer's
    /// worker busy-polls the ring itself: the producer then does nothing.
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

const _: () = assert!(std::mem::size_of::<Control>() <= DATA_OFFSET);

/// Bytes the mapping of a channel with this capacity takes.
pub(crate) fn mapping_len(capacity: usize) -> usize {
    DATA_OFFSET + 2 * capacity
}

/// The largest message a channel of this capacity carries: a quarter of a
/// direction, so that a record always fits even after a wrap.
pub(crate) fn max_message(capacity: usize) -> usize {
    capacity / 4 - HEAD
}

fn padded(len: usize) -> usize {
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
    pub(crate) unsafe fn new(base: *mut u8, capacity: usize, dir: usize) -> Self {
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
    pub(crate) fn pos(&self) -> u64 {
        self.pos
    }

    pub(crate) fn dir_ptr(&self) -> *const Dir {
        self.dir
    }

    pub(crate) fn dir(&self) -> &Dir {
        // SAFETY: valid while the mapping lives (see `new`); every field is
        // an atomic, so the peer writing it at the same time is fine.
        unsafe { &*self.dir }
    }

    /// The consumer's published head, checked against this producer's tail.
    fn checked_head(&self) -> Result<u64, Corrupt> {
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
    fn checked_tail(&self) -> Result<u64, Corrupt> {
        let tail = self.dir().tail.0.load(Ordering::SeqCst);
        let avail = tail.wrapping_sub(self.pos);
        if tail < self.pos || avail > self.capacity || avail % 8 != 0 {
            return Err(Corrupt(
                "the sending process moved its write position out of range",
            ));
        }
        Ok(tail)
    }

    fn write_head(&mut self, off: u64, len: u32) {
        let mut head = [0u8; HEAD];
        head[..4].copy_from_slice(&len.to_le_bytes());
        // SAFETY: `off + 8 <= capacity` (offsets are multiples of 8 and the
        // capacity is too), inside this direction's data area, in space the
        // consumer has released.
        unsafe { std::ptr::copy_nonoverlapping(head.as_ptr(), self.data.add(off as usize), HEAD) };
    }

    /// Producer: appends one message, or says the ring has no room for it.
    /// `msg.len()` must be at most [`max_message`].
    pub(crate) fn push(&mut self, msg: &[u8]) -> Result<Push, Corrupt> {
        debug_assert!(msg.len() <= max_message(self.capacity as usize));
        let total = (HEAD + padded(msg.len())) as u64;
        let off = self.pos % self.capacity;
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
        // Sequentially consistent, as is the producer's next look at
        // `consumer_sleeping`: with the consumer's store of that flag and
        // its next look at `tail` (also sequentially consistent), one of
        // the two sees the other, so a wake-up is never missed.
        self.dir().tail.0.store(pos, Ordering::SeqCst);
        Ok(Push::Done)
    }

    /// Producer: whether a message of `len` bytes would fit now.
    pub(crate) fn has_room(&mut self, len: usize) -> Result<bool, Corrupt> {
        let head = self.checked_head()?;
        self.seen = head;
        let free = self.capacity - (self.pos - head);
        let total = (HEAD + padded(len)) as u64;
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
    pub(crate) fn peek(&mut self) -> Result<Option<(*const u8, usize, u64)>, Corrupt> {
        loop {
            if self.seen == self.pos {
                self.seen = self.checked_tail()?;
            }
            let avail = self.seen - self.pos;
            if avail == 0 {
                return Ok(None);
            }
            let off = self.pos % self.capacity;
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
    pub(crate) fn release(&mut self, total: u64) {
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
    pub(crate) fn has_data(&mut self) -> bool {
        match self.checked_tail() {
            Ok(tail) => {
                self.seen = tail;
                tail != self.pos
            }
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A zeroed, page-aligned stand-in for the shared mapping.
    struct Mem(*mut u8, std::alloc::Layout);

    impl Mem {
        fn new(capacity: usize) -> Self {
            let layout = std::alloc::Layout::from_size_align(mapping_len(capacity), 4096).unwrap();
            // SAFETY: a non-zero size.
            let p = unsafe { std::alloc::alloc_zeroed(layout) };
            assert!(!p.is_null());
            Self(p, layout)
        }
        fn rings(&mut self, capacity: usize) -> (Ring, Ring) {
            // SAFETY: a zeroed, aligned block of the right length that
            // outlives both rings in every test.
            unsafe {
                (
                    Ring::new(self.0, capacity, 0),
                    Ring::new(self.0, capacity, 0),
                )
            }
        }
    }

    impl Drop for Mem {
        fn drop(&mut self) {
            // SAFETY: allocated in `new` with this layout.
            unsafe { std::alloc::dealloc(self.0, self.1) };
        }
    }

    fn pop(r: &mut Ring) -> Option<Vec<u8>> {
        let (ptr, len, total) = r.peek().unwrap()?;
        // SAFETY: as returned by `peek`.
        let v = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        r.release(total);
        Some(v)
    }

    #[test]
    fn messages_come_out_in_order_across_many_wraps() {
        let cap = MIN_CAPACITY;
        let mut mem = Mem::new(cap);
        let (mut tx, mut rx) = mem.rings(cap);
        let mut next_out = 0u32;
        // Fewer under Miri, which is far slower; a few
        // hundred messages still wrap the 4 KiB ring many times.
        let count = if cfg!(miri) { 400 } else { 20_000u32 };
        for i in 0..count {
            let len = (i as usize * 37) % (max_message(cap) + 1);
            let msg: Vec<u8> = (0..len).map(|b| (b as u32 ^ i) as u8).collect();
            while tx.push(&msg).unwrap() == Push::Full {
                let got = pop(&mut rx).unwrap();
                let want_len = (next_out as usize * 37) % (max_message(cap) + 1);
                assert_eq!(got.len(), want_len);
                assert!(got
                    .iter()
                    .enumerate()
                    .all(|(b, &v)| v == (b as u32 ^ next_out) as u8));
                next_out += 1;
            }
        }
        while let Some(got) = pop(&mut rx) {
            assert_eq!(got.len(), (next_out as usize * 37) % (max_message(cap) + 1));
            next_out += 1;
        }
        assert_eq!(next_out, count);
    }

    #[test]
    fn a_full_ring_refuses_until_the_consumer_releases() {
        let cap = MIN_CAPACITY;
        let mut mem = Mem::new(cap);
        let (mut tx, mut rx) = mem.rings(cap);
        let msg = vec![1u8; max_message(cap)];
        let mut n = 0;
        while tx.push(&msg).unwrap() == Push::Done {
            n += 1;
        }
        assert!(n >= 3, "{n}");
        assert!(!tx.has_room(msg.len()).unwrap());
        assert!(pop(&mut rx).is_some());
        assert!(tx.has_room(msg.len()).unwrap());
    }

    #[test]
    fn a_peer_that_moves_positions_out_of_range_is_reported_not_followed() {
        let cap = MIN_CAPACITY;
        let mut mem = Mem::new(cap);
        let (mut tx, mut rx) = mem.rings(cap);
        tx.push(b"hello").unwrap();
        // A sending peer claims more data than the ring holds.
        rx.dir().tail.0.store(cap as u64 * 2, Ordering::Release);
        assert!(rx.peek().is_err());
        // A receiving peer claims to have read past what was written.
        rx.dir().tail.0.store(16, Ordering::Release);
        rx.dir().head.0.store(1 << 40, Ordering::Release);
        // The producer reads `head` again when it needs to know the room.
        assert!(tx.has_room(1).is_err());
    }

    #[test]
    fn a_message_length_past_the_written_data_is_reported() {
        let cap = MIN_CAPACITY;
        let mut mem = Mem::new(cap);
        let (mut tx, mut rx) = mem.rings(cap);
        tx.push(b"hello").unwrap();
        // Overwrite the record head with a huge length.
        // SAFETY: offset 0 of the data area, inside the buffer.
        unsafe { std::ptr::write_unaligned(tx.data.cast::<u32>(), 100_000) };
        assert_eq!(
            rx.peek().unwrap_err(),
            Corrupt("a message length points past the written data")
        );
    }
}
