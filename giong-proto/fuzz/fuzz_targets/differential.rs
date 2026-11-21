//! giong-proto against prost on the same bytes, for both conformance
//! messages (proto3 and proto2): same message when both accept, and any
//! accept/reject difference must be one of the documented ones. See
//! `giong-proto/conformance/src/diff.rs`.
#![no_main]

use giong_proto_conformance::diff::{check_proto2, check_proto3};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(e) = check_proto3(data) {
        panic!("proto3: {e}");
    }
    if let Err(e) = check_proto2(data) {
        panic!("proto2: {e}");
    }
});
