//! The generated code checked into this crate is what giong-grpc-build
//! generates today. Run with `GIONG_GRPC_REGENERATE=1` to rewrite it after
//! a change to the generator or to a `.proto`.

use std::fs;
use std::path::Path;

fn check(protos: &[&str], out: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let include = root.join("proto");
    let files: Vec<_> = protos.iter().map(|p| include.join(p)).collect();
    // One file for each check: the tests run side by side.
    let fresh = std::env::temp_dir().join(format!(
        "giong-grpc-{}-{}",
        std::process::id(),
        out.replace('/', "-")
    ));
    giong_grpc_build::Config::new()
        .compile_to(&files, &[&include], &fresh)
        .unwrap();
    let generated = fs::read_to_string(&fresh).unwrap();
    let _ = fs::remove_file(&fresh);
    let path = root.join(out);
    if std::env::var_os("GIONG_GRPC_REGENERATE").is_some() {
        fs::write(&path, &generated).unwrap();
        return;
    }
    let current = fs::read_to_string(&path).unwrap_or_default();
    assert!(
        current == generated,
        "{out} is out of date: run `GIONG_GRPC_REGENERATE=1 cargo test -p giong-grpc --test \
         generated` and commit the result"
    );
}

#[test]
fn the_interop_test_messages_are_current() {
    check(
        &["grpc/testing/test.proto"],
        "tests/generated/grpc_testing.rs",
    );
}

#[test]
fn the_health_messages_are_current() {
    check(&["grpc/health/v1/health.proto"], "src/generated/health.rs");
}

#[test]
fn the_reflection_messages_are_current() {
    check(
        &[
            "grpc/reflection/v1/reflection.proto",
            "grpc/reflection/v1alpha/reflection.proto",
        ],
        "src/generated/reflection.rs",
    );
}
