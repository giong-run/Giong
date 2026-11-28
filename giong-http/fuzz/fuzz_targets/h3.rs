//! HTTP/3: frame headers, SETTINGS and the one-integer frames, QPACK
//! field sections with the two QPACK streams, and the QPACK dynamic table
//! (an encoder stream, then field sections that refer to it).
//!
//! Checks that reading never panics, never claims more bytes than it was
//! given and only fails cleanly; that what reads writes back to bytes that
//! read the same; and that a field section that decodes encodes (through
//! our encoder) to one that decodes to the same fields.
#![no_main]

use giong_http::fuzzing::{check_h3_frames, check_qpack, check_qpack_table};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Byte 0, bits 0-1: framing, a QPACK field section, or the dynamic
    // table.
    let Some((&how, rest)) = data.split_first() else {
        return;
    };
    match how & 3 {
        0 => {
            check_h3_frames(rest);
        }
        1 => {
            check_qpack(rest);
        }
        _ => {
            check_qpack_table(rest);
        }
    }
});
