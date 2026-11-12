//! The request-head parser: never panics, and everything it accepts can be
//! served (see `giong-http/src/invariants.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first byte picks the offset the terminator search restarts from.
    let Some((&from, input)) = data.split_first() else {
        return;
    };
    giong_http::fuzzing::check_head(input, from as usize);
});
