//! Compressed messages: reading a gzip member never panics and never
//! gives more than the limit, and what giong-grpc compresses it reads
//! back.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [limit, input @ ..] = data else {
        return;
    };
    let max = [0, 1, 64, 4096, 1 << 20][usize::from(*limit) % 5];
    giong_grpc::fuzzing::check_gzip(input, max);
});
