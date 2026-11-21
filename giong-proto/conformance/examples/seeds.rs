//! Writes seed inputs for the `differential` fuzz target:
//! `cargo run --example seeds -- ../fuzz/seeds/differential`.
use giong_proto_conformance::gen;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: seeds <dir>");
    std::fs::create_dir_all(&dir).unwrap();
    for seed in 0..16u64 {
        let t3 = gen::typed_input(
            ".protobuf_test_messages.proto3.TestAllTypesProto3",
            seed * 7 + 1,
        );
        let t2 = gen::typed_input(
            ".protobuf_test_messages.proto2.TestAllTypesProto2",
            seed * 7 + 2,
        );
        std::fs::write(format!("{dir}/proto3-{seed:02}"), t3).unwrap();
        std::fs::write(format!("{dir}/proto2-{seed:02}"), t2).unwrap();
    }
}
