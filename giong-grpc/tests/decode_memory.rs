//! What decoding a hostile message costs in memory: a singular message
//! field sent again and again (which protobuf merges) keeps a slice of the
//! input per occurrence, and no more.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::SimpleRequestView;
use giong_proto::View;

/// Counts the bytes allocated and not freed yet on the measuring thread,
/// and the most at once. Other threads (the test harness, a sanitizer's
/// runtime) allocate too, so only the thread inside `peak_of` is counted.
struct Counting;

thread_local! {
    static MEASURING: Cell<bool> = const { Cell::new(false) };
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn track(delta: isize) {
    // `try_with`: the allocator also runs while thread locals are torn down.
    let _ = MEASURING.try_with(|measuring| {
        if measuring.get() {
            let live = LIVE.with(|live| {
                live.set(live.get() + delta);
                live.get()
            });
            PEAK.with(|peak| peak.set(peak.get().max(live)));
        }
    });
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        track(layout.size() as isize);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        track(-(layout.size() as isize));
        System.dealloc(ptr, layout)
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

/// The most memory decoding `input` held at once on this thread, and the
/// view, to check it was read.
fn peak_of<'a, V: View<'a>>(input: &'a [u8]) -> (usize, V) {
    LIVE.with(|live| live.set(0));
    PEAK.with(|peak| peak.set(0));
    MEASURING.with(|measuring| measuring.set(true));
    let view = V::decode(input).unwrap();
    MEASURING.with(|measuring| measuring.set(false));
    (PEAK.with(Cell::get) as usize, view)
}

fn repeated(occurrence: &[u8], times: usize) -> Vec<u8> {
    occurrence.repeat(times)
}

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
