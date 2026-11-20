//! The descriptor sets given to reflection: indexing one never panics,
//! whatever the bytes, and an index that was made answers.
#![no_main]

use giong_grpc::reflection::Reflection;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(reflection) = Reflection::new(&[data]) {
        let _ = reflection.service_names();
    }
});
