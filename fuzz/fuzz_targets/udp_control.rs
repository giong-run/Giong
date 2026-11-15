//! Control messages of a UDP receive (GRO segment size, ECN bits), which the
//! kernel writes but whose lengths giong must not trust blindly: never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (_segment, ecn) = giong::fuzzing::udp_control(data);
    assert!(ecn <= 3);
});
