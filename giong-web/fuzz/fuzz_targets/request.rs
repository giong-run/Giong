//! Any bytes as a request to an app with every middleware: the answer is
//! well formed whatever they are (see `giong-web/src/fuzzing.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_web::fuzzing::check_request(data);
});
