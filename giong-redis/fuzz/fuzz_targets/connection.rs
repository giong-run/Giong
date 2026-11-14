//! A connection's reply matching, driven by a script with no socket:
//! commands of every shape, dropped waiters, passed deadlines, and push
//! messages, attributes and replies over the limit among the replies. Every
//! command gets its own replies, or the error that is due.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_redis::fuzzing::check_connection(data);
});
