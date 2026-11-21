//! Metadata as it arrives in headers and trailers, binary (`-bin`, base64)
//! values included: never panics, and a field is kept as it came or
//! refused.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The name, a zero byte, the value.
    let Some(split) = data.iter().position(|&b| b == 0) else {
        return;
    };
    giong_grpc::fuzzing::check_metadata(&data[..split], &data[split + 1..]);
});
