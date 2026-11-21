fn main() {
    giong_grpc_build::compile(&["proto/bench.proto"], &["proto"]).unwrap();
}
