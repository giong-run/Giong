//! The server of gRPC's interoperability tests
//! (github.com/grpc/grpc/blob/master/doc/interop-test-descriptions.md):
//! `grpc.testing.TestService`, with health and reflection beside it.
//! Other implementations' interop clients run their test cases against it;
//! `giong-grpc/interop/run.sh` does that with grpc-go's.
//!
//! ```text
//! cargo run -p giong-grpc --example interop_server -- --port=10000 [--compress]
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use giong::net::TcpListener;
use giong_grpc::health::Health;
use giong_grpc::reflection::Reflection;

fn main() {
    let (mut port, mut compress) = (10000u16, false);
    for arg in std::env::args().skip(1) {
        match arg.strip_prefix("--port=").map(str::parse) {
            Some(Ok(given)) => port = given,
            // Responses compressed whenever the client accepts gzip.
            None if arg == "--compress" => compress = true,
            _ => {
                eprintln!("usage: interop_server [--port=10000] [--compress]");
                std::process::exit(2);
            }
        }
    }
    giong::block_on(async move {
        let health = Health::new();
        health.set("grpc.testing.TestService", true);
        let reflection = Reflection::new(&[
            common::pb::FILE_DESCRIPTOR_SET,
            giong_grpc::health::FILE_DESCRIPTOR_SET,
        ])
        .expect("generated descriptors");
        let server = common::test_server()
            .compress(compress)
            .add(health.service())
            .add(reflection.v1())
            .add(reflection.v1alpha());
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
        println!("listening on {}", listener.local_addr().expect("address"));
        giong_http::Server::new().serve(listener, server).await;
    });
}
