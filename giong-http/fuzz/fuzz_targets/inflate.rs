//! gzip and deflate bodies: decoding never panics, never gives more than
//! its limit at once, and ends cleanly or with an error.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_http::fuzzing::check_inflate(data);
});
