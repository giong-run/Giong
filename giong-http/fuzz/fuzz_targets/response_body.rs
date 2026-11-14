//! The client reading an HTTP/1.1 response on a connection: whatever the
//! server sends, in whatever pieces, the head and body are what the bytes
//! hold (the body ending exactly where its length, its chunked framing or
//! the close says), and the connection is reused only when the bytes
//! received end exactly at the end of the body. A byte more would be read
//! as the response to the next request (see `check_response_body` in
//! `giong-http/src/client/conn.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Byte 0: flags (HEAD, how the body is read, how the stream ends, the
    // head limit). Byte 1: seed for the piece sizes. Then the response.
    giong_http::fuzzing::check_response_body(data);
});
