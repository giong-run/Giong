//! A row of 1 to 4 columns of any type, each value read as every Rust type.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_mysql::fuzzing::check_rows(data);
});
