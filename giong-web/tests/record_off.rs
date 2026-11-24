//! A release build without the `dev` feature leaves recording out:
//! `Record::new` panics with a coded error that says why and what to do.
#![cfg(not(any(debug_assertions, feature = "dev")))]

use giong_web::middleware::Record;
use giong_web::Sink;

#[test]
#[should_panic(expected = "GIONG-E1213")]
fn record_refuses_to_be_made_in_a_release_build() {
    let _ = Record::new(Sink::Stdout);
}

#[test]
fn the_refusal_names_the_cause_and_the_fix() {
    let err = std::panic::catch_unwind(|| Record::new(Sink::Stdout)).unwrap_err();
    let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(msg.contains("release build"), "{msg}");
    assert!(msg.contains("--features giong-web/dev"), "{msg}");
}
