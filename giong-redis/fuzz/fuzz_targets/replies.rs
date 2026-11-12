//! RESP3 replies, cut anywhere and read with any size limit: reading ends
//! with replies or an error, never a panic, and the limit holds.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_redis::fuzzing::check_replies(data);
});
