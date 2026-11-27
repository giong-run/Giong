//! Handler futures on the heap, in memory the worker reuses.
//!
//! The server keeps a handler's future on the heap for the length of one
//! request, so that an idle connection does not carry the largest future a
//! handler can make. Requests come and go in batches, though: a turn of the
//! event loop can start and finish a request on dozens of connections, far
//! more than the allocator's own per-thread cache keeps (seven blocks of each
//! size in glibc), and its slow path costs more than the rest of the
//! bookkeeping of a small request. So the blocks go back to a cache of this
//! worker's instead, bounded in bytes.

use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::cell::UnsafeCell;
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::task::{Context, Poll};

/// Most bytes of free blocks kept for each block size.
const KEEP_BYTES: usize = 256 * 1024;
/// Free blocks kept for each size, at least (for very large futures).
const KEEP_MIN: usize = 4;

/// Free blocks of one size.
struct Blocks {
    layout: Layout,
    /// How many free blocks of this size are kept at most.
    keep: usize,
    free: Vec<NonNull<u8>>,
}

/// The free blocks of this thread, by size. A process has one entry per
/// handler type, so a short list is searched.
struct Cache(Vec<Blocks>);

impl Drop for Cache {
    fn drop(&mut self) {
        for blocks in &self.0 {
            for &block in &blocks.free {
                // SAFETY: allocated in `take` with exactly this layout.
                unsafe { dealloc(block.as_ptr(), blocks.layout) };
            }
        }
    }
}

thread_local! {
    static CACHE: UnsafeCell<Cache> = const { UnsafeCell::new(Cache(Vec::new())) };
}

/// Runs `f` on this thread's cache, unless the thread is being torn down
/// and it is gone. An `UnsafeCell` rather than a `RefCell`: this runs twice
/// for every request, and the borrow flag was a sixth of it.
#[inline(always)]
fn with_cache<R>(f: impl FnOnce(&mut Cache) -> R) -> Option<R> {
    CACHE
        .try_with(|cache| {
            // SAFETY: the cache belongs to this thread, and the only callers
            // (`take`, `give`, `give_slowly` and a test) neither keep the
            // reference past `f` nor reach this function again from inside
            // it: what `f` calls is `Vec`'s own code and the global
            // allocator, which cannot get to this module's private
            // functions. So no other reference to the cache exists while
            // `f` runs.
            f(unsafe { &mut *cache.get() })
        })
        .ok()
}

/// A block for `layout` (whose size is not zero).
fn take(layout: Layout) -> NonNull<u8> {
    let reused = with_cache(|cache| {
        cache
            .0
            .iter_mut()
            .find(|b| b.layout == layout)
            .and_then(|b| b.free.pop())
    })
    .flatten();
    match reused {
        Some(block) => block,
        None => allocate(layout),
    }
}

/// A new block for `layout`, from the allocator.
#[cold]
#[inline(never)]
fn allocate(layout: Layout) -> NonNull<u8> {
    // SAFETY: the size is not zero (checked by the caller of `take`).
    NonNull::new(unsafe { alloc(layout) }).unwrap_or_else(|| handle_alloc_error(layout))
}

/// Gives a block from [`take`] back.
///
/// Kept apart from the rest: the usual case, a block for a size already
/// seen whose list has room, calls nothing.
fn give(block: NonNull<u8>, layout: Layout) {
    let kept = with_cache(
        |cache| match cache.0.iter_mut().find(|b| b.layout == layout) {
            // The list never grows past `keep` (see `give_slowly`).
            Some(b) if b.free.len() < b.free.capacity() => {
                b.free.push(block);
                true
            }
            _ => false,
        },
    )
    .unwrap_or(false);
    if !kept {
        give_slowly(block, layout);
    }
}

/// [`give`] for a size not seen yet, or a list that has to grow or is full.
#[cold]
#[inline(never)]
fn give_slowly(block: NonNull<u8>, layout: Layout) {
    let kept = with_cache(|cache| {
        let index = match cache.0.iter().position(|b| b.layout == layout) {
            Some(index) => index,
            None => {
                cache.0.push(Blocks {
                    layout,
                    keep: (KEEP_BYTES / layout.size()).max(KEEP_MIN),
                    free: Vec::new(),
                });
                cache.0.len() - 1
            }
        };
        let blocks = &mut cache.0[index];
        let len = blocks.free.len();
        if len < blocks.keep {
            if len == blocks.free.capacity() {
                // Room for as many again, up to `keep`: `give` pushes
                // only while there is room.
                blocks.free.reserve_exact(len.max(4).min(blocks.keep - len));
            }
            blocks.free.push(block);
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if !kept {
        // SAFETY: allocated in `take` with exactly this layout.
        unsafe { dealloc(block.as_ptr(), layout) };
    }
}

/// A future that lives on the heap, in a block from this thread's cache.
pub(crate) struct HeapFuture<F> {
    ptr: NonNull<F>,
}

impl<F: Future> HeapFuture<F> {
    /// The future `make` returns, made where it is to live: an `async fn`'s
    /// future that has not started holds little more than its arguments,
    /// and made in its block it is not copied there whole.
    #[inline(always)]
    pub(crate) fn with(make: impl FnOnce() -> F) -> Self {
        let layout = Layout::new::<F>();
        let ptr = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            take(layout).cast::<F>()
        };
        // Gives the block back if `make` panics (a handler may, before its
        // future exists, and the server catches that panic).
        let release = Release(ptr.cast(), layout);
        // SAFETY: `ptr` is valid for writes of an `F` and properly aligned.
        unsafe { ptr.as_ptr().write(make()) };
        mem::forget(release);
        Self { ptr }
    }
}

/// Gives a block back when dropped, even while a panic unwinds.
struct Release(NonNull<u8>, Layout);

impl Drop for Release {
    fn drop(&mut self) {
        if self.1.size() != 0 {
            give(self.0, self.1);
        }
    }
}

// The future never moves: it stays where `with` put it until `drop`.
impl<F> Unpin for HeapFuture<F> {}

impl<F: Future> Future for HeapFuture<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        // SAFETY: the future is only ever reached through this pinned
        // reference and stays at `ptr` until it is dropped in place.
        unsafe { Pin::new_unchecked(&mut *self.ptr.as_ptr()) }.poll(cx)
    }
}

impl<F> Drop for HeapFuture<F> {
    fn drop(&mut self) {
        // Returns the block even when the future's destructor panics.
        let _release = Release(self.ptr.cast(), Layout::new::<F>());
        // SAFETY: the future was written in `with` and is dropped only here.
        unsafe { ptr::drop_in_place(self.ptr.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::task::{Wake, Waker};

    struct Noop;

    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    fn noop() -> Waker {
        Waker::from(Arc::new(Noop))
    }

    struct Counted(Rc<Cell<usize>>, [u8; 100]);

    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn runs_drops_and_reuses_blocks() {
        let drops = Rc::new(Cell::new(0));
        let mut addrs = Vec::new();
        for i in 0..3u8 {
            let counted = Counted(drops.clone(), [i; 100]);
            let mut fut = HeapFuture::with(|| async move {
                let counted = counted;
                counted.1[0]
            });
            addrs.push(fut.ptr.as_ptr() as usize);
            let waker = noop();
            let mut cx = Context::from_waker(&waker);
            assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(i));
        }
        // The block of the first future is used again for the next ones.
        assert!(addrs.windows(2).all(|w| w[0] == w[1]), "{addrs:?}");
        assert_eq!(drops.get(), 3);
    }

    #[test]
    fn drops_a_future_that_never_ran() {
        let drops = Rc::new(Cell::new(0));
        let counted = Counted(drops.clone(), [0; 100]);
        drop(HeapFuture::with(|| async move {
            let counted = counted;
            counted.1[0]
        }));
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn gives_the_block_back_when_making_the_future_panics() {
        type Fut = std::future::Ready<[u8; 1000]>;
        let layout = Layout::new::<Fut>();
        let free = || {
            with_cache(|cache| {
                cache
                    .0
                    .iter()
                    .find(|b| b.layout == layout)
                    .map_or(0, |b| b.free.len())
            })
            .expect("the cache")
        };
        drop(HeapFuture::with(|| std::future::ready([0u8; 1000])));
        assert_eq!(free(), 1);
        let made = std::panic::catch_unwind(|| HeapFuture::with(|| -> Fut { panic!("no future") }));
        assert!(made.is_err());
        assert_eq!(free(), 1);
    }

    #[test]
    fn handles_zero_sized_futures() {
        let mut fut = HeapFuture::with(|| std::future::ready(()));
        let waker = noop();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn keeps_a_bounded_number_of_blocks() {
        let futs: Vec<_> = (0..10_000)
            .map(|_| HeapFuture::with(|| std::future::ready([0u8; 4096])))
            .collect();
        drop(futs);
        let kept = with_cache(|cache| {
            let blocks = cache
                .0
                .iter()
                .find(|b| b.layout == Layout::new::<std::future::Ready<[u8; 4096]>>())
                .expect("blocks of that size");
            blocks.free.len()
        })
        .expect("the cache");
        let size = Layout::new::<std::future::Ready<[u8; 4096]>>().size();
        assert_eq!(kept, KEEP_BYTES / size);
    }
}
