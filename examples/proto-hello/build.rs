fn main() {
    // Parses the .proto in Rust (no protoc) and writes $OUT_DIR/giong_proto.rs.
    // Messages only: this program does not use giong-grpc, so the service in
    // the .proto is described (its name, its methods' paths) and nothing more.
    giong_grpc_build::Config::new()
        .services(false)
        .compile(&["proto/shop.proto"], &["proto"])
        .unwrap();
}
