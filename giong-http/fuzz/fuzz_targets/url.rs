//! URLs and redirect locations: never panics; what parses prints back the
//! same, and a resolved location is a valid request target.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (url, location) = text.split_once('\n').unwrap_or((&text, "/"));
    giong_http::fuzzing::check_url(url, location);
});
