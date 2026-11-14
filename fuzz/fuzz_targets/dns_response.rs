//! DNS answers, as a server or anyone on the path to it can send them: never
//! panic, and never take more than linear time (the CI job runs this with a
//! short `-timeout`, which catches answers that freeze the worker).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong::fuzzing::dns_response(data);
});
