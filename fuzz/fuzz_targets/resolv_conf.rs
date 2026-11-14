//! `/etc/resolv.conf` and `/etc/hosts`, which a container runtime or an
//! administrator writes: never panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong::fuzzing::resolv_conf(data);
});
