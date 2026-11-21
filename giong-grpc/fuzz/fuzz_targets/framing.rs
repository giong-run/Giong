//! The framing of gRPC messages in a body (a flag byte, a length, the
//! message, gzip or not): never panics, respects the message limit, and
//! gives the same messages and the same failure however the body is split
//! into pieces.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Byte 0: the message limit and whether gzip was announced. Byte 1:
    // seed for the piece sizes.
    let [limit, seed, input @ ..] = data else {
        return;
    };
    let max = [0, 1, 5, 64, 4096, 1 << 20, 4 << 20][usize::from(*limit & 0x7f) % 7];
    let gzip = limit & 0x80 != 0;
    let mut state = u32::from(*seed) | 1;
    giong_grpc::fuzzing::check_framing(input, max, gzip, || {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        state as usize
    });
});
