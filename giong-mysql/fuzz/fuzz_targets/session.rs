//! A whole connection: a client with requests from several tasks in flight
//! (pipelined, one given up, a batch, a reset, a transaction) reads what a
//! server sends, in arbitrary reads.
//!
//! Checks that the client never panics and always finishes (libFuzzer's
//! `-timeout` catches a hang), that every request ends, and that what the
//! client writes is a sequence of well-formed commands.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_mysql::fuzzing::check_session(data);
});
