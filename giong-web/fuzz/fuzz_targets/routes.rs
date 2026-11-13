//! Routes and a request path built from the input: the route table finds
//! the route a slow reference matcher finds, with the same parameters, or
//! the same 404, 405 or OPTIONS answer (see `giong-web/src/route/model.rs`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    giong_web::fuzzing::check_routes(data);
});
