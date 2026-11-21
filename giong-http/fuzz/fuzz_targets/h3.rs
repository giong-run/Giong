//! HTTP/3: frame headers, SETTINGS and the one-integer frames, and QPACK
//! field sections with the two QPACK streams.
//!
//! Checks that reading never panics, never claims more bytes than it was
//! given and only fails cleanly; that what reads writes back to bytes that
//! read the same; and that a field section that decodes encodes (through
//! our encoder) to one that decodes to the same fields.
#![no_main]

use giong_http::fuzzing::{check_h3_frames, check_qpack};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Byte 0, bit 0: framing, or a QPACK field section.
    let Some((&how, rest)) = data.split_first() else {
        return;
    };
    if how & 1 == 0 {
        check_h3_frames(rest);
    } else {
        check_qpack(rest);
    }
});
