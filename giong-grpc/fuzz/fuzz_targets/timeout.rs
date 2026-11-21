//! The `grpc-timeout` header: parsing never panics and accepts only what
//! the protocol allows, and a timeout written for sending reads back as no
//! shorter than it was.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_grpc::fuzzing::check_timeout(data);
});
