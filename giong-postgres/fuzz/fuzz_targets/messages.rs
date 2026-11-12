//! Server messages, framed and cut anywhere, through every parser that
//! reads them (messages, errors, rows): a verdict, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_postgres::fuzzing::check_messages(data);
});
