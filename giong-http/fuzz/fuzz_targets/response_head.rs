//! The client's response-head parser: never panics, and the ranges and
//! framing of what it accepts hold (see `giong-http/src/client/mod.rs`).
//! The same bytes go through the `Alt-Svc` parser, which reads a header
//! field of those heads.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_http::fuzzing::check_response_head(data);
    giong_http::fuzzing::check_alt_svc(data);
});
