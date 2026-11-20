fn main() {
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    giong_grpc_build::Config::new()
        .compile_to(
            &["proto/bench.proto"],
            &["proto"],
            &out.join("giong_proto.rs"),
        )
        .unwrap();
    let fds = protox::compile(["proto/bench.proto"], ["proto"]).unwrap();
    // prost's defaults (String, Vec<u8>, HashMap), as tonic-build uses them.
    prost_build::Config::new()
        .out_dir(&out)
        .compile_fds(fds)
        .unwrap();
    println!("cargo:rerun-if-changed=proto/bench.proto");
}
