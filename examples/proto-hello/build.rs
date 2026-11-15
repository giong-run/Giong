fn main() {
    // Parses the .proto in Rust (no protoc) and writes $OUT_DIR/giong_proto.rs.
    giong_grpc_build::compile(&["proto/shop.proto"], &["proto"]).unwrap();
}
