//! The chunked body decoder: never panics, respects the body limit, and gives
//! the same answer however the input is split into pieces.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Byte 0: the body limit. Byte 1: seed for the piece sizes.
    let [limit, seed, input @ ..] = data else {
        return;
    };
    let max = [0, 1, 5, 11, 64, 4096, 1 << 20][*limit as usize % 7];
    let mut state = u32::from(*seed) | 1;
    giong_http::fuzzing::check_chunked(input, max, || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as usize
    });
});
