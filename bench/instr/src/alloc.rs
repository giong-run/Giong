//! The benchmark's global allocator: it counts allocations and live bytes,
//! and for `run` it serves memory from size classes of its own.
//!
//! Why not glibc's malloc under callgrind: what malloc and free execute
//! depends on the state of the heap (whether a freed chunk merges with its
//! neighbour, whether a bin has to be searched or consolidated), and that
//! state depends on everything allocated since the process started, down to
//! the length of the program's path. The same binary measured 12,304 or
//! 12,620 instructions per `postgres-query` from two directories, all of the
//! difference inside malloc (`bench/results/instr/conditions.txt`, 5). A
//! change to unrelated code moves the heap the same way, and the gate failed
//! pull requests that had not touched the case.
//!
//! Here an allocation costs the same instructions whatever came before: a
//! size class from the size (powers of two from 16 bytes to 1 MiB), a pop
//! from that class's free list or a step of the bump pointer, and a push on
//! free. A `realloc` that stays in its class returns the same block, and one
//! that leaves it copies, like a `Vec` growing past its capacity. The count
//! of a case then depends on the sizes it asks for and in which order,
//! which is its own behaviour, and how many allocations it makes is a metric
//! of its own.
//!
//! The memory cases (`idle`) keep glibc's malloc, since the resident set
//! they report is the allocator's as much as giong's.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicUsize = AtomicUsize::new(0);

pub fn allocations() -> u64 {
    ALLOCS.load(Relaxed)
}

pub fn live_bytes() -> usize {
    LIVE.load(Relaxed)
}

/// The smallest class is 16 bytes, the largest 1 MiB; larger blocks, and
/// alignments above a page, go to the system.
const MIN_SHIFT: u32 = 4;
const MAX_SHIFT: u32 = 20;
const CLASSES: usize = (MAX_SHIFT - MIN_SHIFT + 1) as usize;
const PAGE: usize = 4096;
/// Address space for the classes, reserved once; only touched pages are
/// backed by memory.
const ARENA: usize = 4 << 30;

struct Classes {
    /// The arena's bounds; 0 until `fixed_cost` reserved it.
    base: usize,
    end: usize,
    /// The first byte never handed out.
    bump: usize,
    /// Per class, the most recently freed block; each free block holds the
    /// address of the next in its first word.
    free: [usize; CLASSES],
}

pub struct Counting {
    /// Whether allocations are served from the classes.
    fixed: AtomicBool,
    lock: AtomicBool,
    classes: UnsafeCell<Classes>,
}

// SAFETY: `classes` is only reached through `with`, which holds `lock`.
unsafe impl Sync for Counting {}

impl Counting {
    pub const fn new() -> Self {
        Counting {
            fixed: AtomicBool::new(false),
            lock: AtomicBool::new(false),
            classes: UnsafeCell::new(Classes {
                base: 0,
                end: 0,
                bump: 0,
                free: [0; CLASSES],
            }),
        }
    }

    /// From now on, serves allocations from the size classes. Memory
    /// allocated before stays the system's and is freed there.
    pub fn fixed_cost(&self) {
        // SAFETY: an anonymous private mapping at an address the kernel
        // picks; nothing else refers to it.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                ARENA,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        assert!(base != libc::MAP_FAILED, "reserve the allocator's arena");
        self.with(|c| {
            c.base = base as usize;
            c.end = base as usize + ARENA;
            c.bump = base as usize;
        });
        self.fixed.store(true, Release);
    }

    /// Runs `f` with the classes locked. The measured thread is alone, so
    /// the lock is free and costs the same every time; a helper thread (the
    /// epoll driver's file threads) can make it spin.
    fn with<R>(&self, f: impl FnOnce(&mut Classes) -> R) -> R {
        while self
            .lock
            .compare_exchange_weak(false, true, Acquire, Relaxed)
            .is_err()
        {
            std::hint::spin_loop();
        }
        // SAFETY: the lock is held, so this is the only reference.
        let r = f(unsafe { &mut *self.classes.get() });
        self.lock.store(false, Release);
        r
    }

    /// The class of a layout, if the classes serve it.
    fn class(layout: Layout) -> Option<usize> {
        if layout.align() > PAGE {
            return None;
        }
        let size = layout.size().max(layout.align()).max(1 << MIN_SHIFT);
        let shift = usize::BITS - (size - 1).leading_zeros();
        (shift <= MAX_SHIFT).then(|| (shift - MIN_SHIFT) as usize)
    }

    /// A block of `class`, or null when the arena is used up.
    fn take(&self, class: usize) -> *mut u8 {
        self.with(|c| {
            let head = c.free[class];
            if head != 0 {
                // SAFETY: a free block of this class holds the next free
                // block's address in its first word (`give`), and is at
                // least 16 bytes, aligned to 16.
                c.free[class] = unsafe { *(head as *const usize) };
                return head as *mut u8;
            }
            let size = 1usize << (class as u32 + MIN_SHIFT);
            // A block is aligned to its size, up to a page: enough for any
            // layout of the class.
            let align = size.min(PAGE);
            let at = (c.bump + align - 1) & !(align - 1);
            if at + size > c.end {
                return std::ptr::null_mut();
            }
            c.bump = at + size;
            at as *mut u8
        })
    }

    fn give(&self, ptr: *mut u8, class: usize) {
        self.with(|c| {
            // SAFETY: the block is free from here on, at least 16 bytes and
            // aligned to 16: its first word is ours to write.
            unsafe { *(ptr as *mut usize) = c.free[class] };
            c.free[class] = ptr as usize;
        })
    }

    /// Whether `ptr` came from the classes. The bounds are written once,
    /// before `fixed` is set, and a block can only exist after that.
    fn owns(&self, ptr: *mut u8) -> bool {
        if !self.fixed.load(Acquire) {
            return false;
        }
        // SAFETY: `base` and `end` do not change once `fixed` is set.
        let (base, end) = unsafe { ((*self.classes.get()).base, (*self.classes.get()).end) };
        (base..end).contains(&(ptr as usize))
    }

    fn fixed_class(&self, layout: Layout) -> Option<usize> {
        if self.fixed.load(Acquire) {
            Self::class(layout)
        } else {
            None
        }
    }
}

// SAFETY: a block comes either from `System`, with the layout asked for, or
// from a class whose blocks are at least as large as the layout and aligned
// to at least its alignment; no block is handed out twice (a class's free
// list holds only blocks given back, the bump pointer only moves forward),
// and a block returns to where it came from (`owns`).
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE.fetch_add(layout.size(), Relaxed);
        match self.fixed_class(layout) {
            Some(class) => self.take(class),
            // SAFETY: forwarded as received.
            None => unsafe { System.alloc(layout) },
        }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE.fetch_add(layout.size(), Relaxed);
        match self.fixed_class(layout) {
            Some(class) => {
                let ptr = self.take(class);
                if !ptr.is_null() {
                    // SAFETY: the block is at least `layout.size()` bytes.
                    unsafe { ptr.write_bytes(0, layout.size()) };
                }
                ptr
            }
            // SAFETY: forwarded as received.
            None => unsafe { System.alloc_zeroed(layout) },
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        if self.owns(ptr) {
            // A block of ours was allocated with this layout, so it has a
            // class.
            self.give(ptr, Self::class(layout).expect("a class"));
        } else {
            // SAFETY: not ours, so it came from `System` with this layout.
            unsafe { System.dealloc(ptr, layout) }
        }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        LIVE.fetch_sub(layout.size(), Relaxed);
        LIVE.fetch_add(new_size, Relaxed);
        if !self.owns(ptr) && !self.fixed.load(Acquire) {
            // SAFETY: forwarded as received.
            return unsafe { System.realloc(ptr, layout, new_size) };
        }
        // SAFETY: the caller guarantees that `new_size` is a valid size for
        // this alignment.
        let new = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        if self.owns(ptr) && Self::class(new) == Self::class(layout) {
            return ptr;
        }
        let to = match self.fixed_class(new) {
            Some(class) => self.take(class),
            // SAFETY: a valid layout, as above.
            None => unsafe { System.alloc(new) },
        };
        if to.is_null() {
            return to;
        }
        // SAFETY: both blocks are at least this long and distinct.
        unsafe { std::ptr::copy_nonoverlapping(ptr, to, layout.size().min(new_size)) };
        if self.owns(ptr) {
            self.give(ptr, Self::class(layout).expect("a class"));
        } else {
            // SAFETY: not ours, so it came from `System` with this layout.
            unsafe { System.dealloc(ptr, layout) }
        }
        to
    }
}
