fn main() {
    // The same .proto as giong's, parsed in Rust: no protoc.
    let fds =
        protox::compile(["../grpc-giong/proto/bench.proto"], ["../grpc-giong/proto"]).unwrap();
    tonic_prost_build::configure().compile_fds(fds).unwrap();
    println!("cargo:rerun-if-changed=../grpc-giong/proto/bench.proto");
}
