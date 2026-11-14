//! A connection talking to a server that answers each of its requests with
//! arbitrary bytes: every statement gets a result, nothing panics, and
//! nothing waits once the server has closed the connection.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_postgres::fuzzing::check_session(data);
});
