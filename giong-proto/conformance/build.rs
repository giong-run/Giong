// Generates the conformance messages twice: once with giong-grpc-build (the
// code under test) and once with prost-build (the reference for the
// differential tests). Both from the same descriptors, parsed by protox, so
// no protoc is needed.
fn main() {
    let files = [
        "proto/conformance.proto",
        "proto/test_messages_proto3.proto",
        "proto/test_messages_proto2.proto",
    ];
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    giong_grpc_build::compile_to(&files, &["proto"], &out.join("giong_proto.rs")).unwrap();
    let fds = protox::compile(files, ["proto"]).unwrap();
    prost_build::Config::new()
        .btree_map(["."])
        .out_dir(&out)
        .compile_fds(fds)
        .unwrap();
    println!("cargo:rerun-if-changed=build.rs");
}
