//! Heap an idle `Compat` keeps after writing: this binary counts every
//! allocation, so it holds this test alone.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::time::Duration;

use common::*;
use giong::time::sleep;
use giong_compat::Compat;
use tokio::io::AsyncWriteExt;

struct Counting;

static LIVE: AtomicIsize = AtomicIsize::new(0);

// SAFETY: every call is passed on to the system allocator unchanged; only
// the count of live bytes is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE.fetch_add(layout.size() as isize, Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as isize, Relaxed);
        System.dealloc(ptr, layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        LIVE.fetch_add(size as isize - layout.size() as isize, Relaxed);
        System.realloc(ptr, layout, size)
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Live heap a flushed `Compat` frees when it hands its stream back: what
/// it held while idle, after `total` bytes written in `piece`-byte writes.
async fn held_when_idle(total: usize, piece: usize) -> isize {
    let (client, server) = pair().await;
    let reader = giong::spawn(async move { while !server.recv().await.unwrap().is_empty() {} });
    let mut c = Compat::new(client);
    let data = vec![9u8; piece];
    for _ in 0..total / piece {
        c.write_all(&data).await.unwrap();
    }
    c.flush().await.unwrap();
    sleep(Duration::from_millis(20)).await;
    let before = LIVE.load(Relaxed);
    let (tcp, _) = c.into_inner().await.unwrap();
    let held = before - LIVE.load(Relaxed);
    drop(tcp);
    reader.await.unwrap();
    held
}

#[test]
fn an_idle_compat_holds_no_write_buffer() {
    giong::block_on(async {
        for (total, piece) in [
            (100, 100),
            (16 << 10, 1 << 10),
            (1 << 20, 16 << 10),
            (8 << 20, 64 << 10),
            (8 << 20, 1000),
        ] {
            let held = held_when_idle(total, piece).await;
            // The stream's `Rc` and the adapter's small state; a kept write
            // buffer is up to 64 KiB.
            assert!(
                held < 1024,
                "{held} bytes held while idle after {total} bytes in {piece}-byte writes"
            );
        }
    });
}
