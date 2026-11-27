//! The input gzipped by `Compression`'s compressor, at the level and with the
//! number of copies its first byte picks: it decompresses to the same bytes,
//! through libdeflate (the `fast-gzip` path) and miniz_oxide alike (see
//! `giong-web/src/fuzzing.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_web::fuzzing::check_gzip(data);
});
