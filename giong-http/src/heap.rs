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
use std::cell::RefCell;
use std::future::Future;
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
    static CACHE: RefCell<Cache> = const { RefCell::new(Cache(Vec::new())) };
}

/// A block for `layout` (whose size is not zero).
fn take(layout: Layout) -> NonNull<u8> {
    let reused = CACHE
        .try_with(|cache| {
            let mut cache = cache.borrow_mut();
            cache
                .0
                .iter_mut()
                .find(|b| b.layout == layout)
                .and_then(|b| b.free.pop())
        })
        .ok()
        .flatten();
    if let Some(block) = reused {
        return block;
    }
    // SAFETY: the size is not zero (checked by the caller).
    NonNull::new(unsafe { alloc(layout) }).unwrap_or_else(|| handle_alloc_error(layout))
}

/// Gives a block from [`take`] back.
fn give(block: NonNull<u8>, layout: Layout) {
    let kept = CACHE
        .try_with(|cache| {
            let mut cache = cache.borrow_mut();
            let index = match cache.0.iter().position(|b| b.layout == layout) {
                Some(index) => index,
                None => {
                    cache.0.push(Blocks {
                        layout,
                        free: Vec::new(),
                    });
                    cache.0.len() - 1
                }
            };
            let blocks = &mut cache.0[index];
            if blocks.free.len() < (KEEP_BYTES / layout.size()).max(KEEP_MIN) {
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
    pub(crate) fn new(fut: F) -> Self {
        let layout = Layout::new::<F>();
        let ptr = if layout.size() == 0 {
            NonNull::dangling()
        } else {
            take(layout).cast::<F>()
        };
        // SAFETY: `ptr` is valid for writes of an `F` and properly aligned.
        unsafe { ptr.as_ptr().write(fut) };
        Self { ptr }
    }
}

// The future never moves: it stays where `new` put it until `drop`.
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
        /// Returns the block even when the future's destructor panics.
        struct Release(NonNull<u8>, Layout);
        impl Drop for Release {
            fn drop(&mut self) {
                if self.1.size() != 0 {
                    give(self.0, self.1);
                }
            }
        }
        let _release = Release(self.ptr.cast(), Layout::new::<F>());
        // SAFETY: the future was written in `new` and is dropped only here.
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
            let mut fut = HeapFuture::new(async move {
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
        drop(HeapFuture::new(async move {
            let counted = counted;
            counted.1[0]
        }));
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn handles_zero_sized_futures() {
        let mut fut = HeapFuture::new(std::future::ready(()));
        let waker = noop();
        let mut cx = Context::from_waker(&waker);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(()));
    }

    #[test]
    fn keeps_a_bounded_number_of_blocks() {
        let futs: Vec<_> = (0..10_000)
            .map(|_| HeapFuture::new(std::future::ready([0u8; 4096])))
            .collect();
        drop(futs);
        let kept = CACHE.with(|cache| {
            let cache = cache.borrow();
            let blocks = cache
                .0
                .iter()
                .find(|b| b.layout == Layout::new::<std::future::Ready<[u8; 4096]>>())
                .expect("blocks of that size");
            blocks.free.len()
        });
        let size = Layout::new::<std::future::Ready<[u8; 4096]>>().size();
        assert_eq!(kept, KEEP_BYTES / size);
    }
}
