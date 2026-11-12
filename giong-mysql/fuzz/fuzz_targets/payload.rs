//! One packet's payload through every parser of server packets
//! (handshake, OK, ERR, column definitions...): a verdict, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_mysql::fuzzing::check_payload(data);
});
