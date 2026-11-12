//! `mysql://` URLs: an error or a usable config, never a panic.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_mysql::fuzzing::check_url(data);
});
