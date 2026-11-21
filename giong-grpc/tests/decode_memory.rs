//! What decoding a hostile message costs in memory: a singular message
//! field sent again and again (which protobuf merges) keeps a slice of the
//! input per occurrence, and no more.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use common::SimpleRequestView;
use giong_proto::View;

/// Counts the bytes allocated and not freed yet, and the most at once.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
        PEAK.fetch_max(live, Ordering::SeqCst);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The most memory decoding `input` held at once beyond what was live
/// before, and the view, to check it was read.
fn peak_of<'a, V: View<'a>>(input: &'a [u8]) -> (usize, V) {
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let view = V::decode(input).unwrap();
    (PEAK.load(Ordering::SeqCst) - before, view)
}

fn repeated(occurrence: &[u8], times: usize) -> Vec<u8> {
    occurrence.repeat(times)
}

// One test function: the counter is global, and tests run side by side.
#[test]
fn a_repeated_singular_message_costs_a_bounded_multiple_of_its_input() {
    // Field 3 (`payload`), empty, 100 000 times: nothing to keep.
    let input = repeated(&[0x1a, 0x00], 100_000);
    let (peak, view) = peak_of::<SimpleRequestView>(&input);
    assert!(view.payload().is_some());
    assert!(
        peak <= 64,
        "{peak} bytes for {} bytes of input",
        input.len()
    );

    // The smallest occurrence with something in it: `payload { type: 0 }`,
    // four bytes on the wire, a slice of 16 bytes kept for it.
    let input = repeated(&[0x1a, 0x02, 0x08, 0x00], 100_000);
    let (peak, view) = peak_of::<SimpleRequestView>(&input);
    assert!(view.payload().is_some());
    assert!(
        peak <= 8 * input.len(),
        "{peak} bytes for {} bytes of input",
        input.len()
    );
}
