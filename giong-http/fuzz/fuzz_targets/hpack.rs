//! HPACK: header blocks, one after another on the same decoder as on a
//! connection, and Huffman-coded strings.
//!
//! Checks that decoding never panics and only fails cleanly, that what
//! decodes encodes back (through our encoder) to a block that decodes to
//! the same fields, that the dynamic tables stay within their limits, and
//! that Huffman decoding accepts exactly what encoding can produce.
#![no_main]

use giong_http::fuzzing::{check_hpack, check_huffman};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Byte 0, bit 0: a sequence of header blocks, or one Huffman string.
    let Some((&how, rest)) = data.split_first() else {
        return;
    };
    if how & 1 == 0 {
        check_hpack(rest);
    } else {
        check_huffman(rest);
    }
});
