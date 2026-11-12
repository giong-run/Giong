//! Packets framed from bytes cut anywhere, with a size limit that holds.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_mysql::fuzzing::check_framing(data);
});
