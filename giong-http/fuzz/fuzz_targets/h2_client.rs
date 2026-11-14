//! The client's side of an HTTP/2 connection: frames a server might send,
//! fed in pieces while requests come and go. Checks that the client never
//! panics, holds no more than the windows it grants, and writes only
//! well-formed frames within the server's windows (see
//! `giong-http/src/client/h2.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_http::fuzzing::check_h2_client(data);
});
