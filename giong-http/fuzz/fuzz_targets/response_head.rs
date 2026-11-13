//! The client's response-head parser: never panics, and the ranges and
//! framing of what it accepts hold (see `giong-http/src/client/mod.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_http::fuzzing::check_response_head(data);
});
