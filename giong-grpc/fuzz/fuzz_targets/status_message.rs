//! The `grpc-message` trailer (percent-encoded UTF-8): decoding never
//! panics and never refuses, and any text survives encoding and decoding.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_grpc::fuzzing::check_status_message(data);
});
