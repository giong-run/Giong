//! The server's SCRAM-SHA-256 messages: refused or used, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_postgres::fuzzing::check_scram(data);
});
