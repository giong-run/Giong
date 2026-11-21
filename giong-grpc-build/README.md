# giong-grpc-build

Generates [giong-proto](../giong-proto) messages and [giong-grpc](../giong-grpc) services from `.proto` files at build time. The
`.proto` files are parsed in Rust by [protox](https://docs.rs/protox) (Google's
well-known types included), so there is **no `protoc` to install**: `cargo build` is
enough, on any machine an agent or a CI runner starts with.

> **New in giong 1.1.** Not published before giong 1.0.0 is released.

```rust
// build.rs
fn main() {
    giong_grpc_build::compile(&["proto/shop.proto"], &["proto"]).unwrap();
}
```

```rust
// src/main.rs
mod pb {
    include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
}
```

The output is one file of plain Rust (no macros, nothing hidden): a module per package,
and per message a view `MView<'a>` for reading, a struct `M<'a>` for writing, its nested
types in module `m`, a Rust enum per protobuf enum, and per service a module with the
paths and kinds of its methods, a trait to implement (one method for each rpc), a
`...Server` that routes calls to an implementation of the trait, and a `...Client`.
[giong-proto's README](../giong-proto/README.md) describes the generated messages and
[giong-grpc's](../giong-grpc/README.md) the services;
[`examples/proto-hello`](../examples/proto-hello) and
[`examples/grpc-hello`](../examples/grpc-hello) use them.

The file also holds `FILE_DESCRIPTOR_SET`, the `.proto` files themselves in encoded form,
which `giong_grpc::reflection` serves to tools such as `grpcurl`. A program that does not
use the constant does not carry it.

A crate that only needs the messages, without a dependency on giong-grpc, turns the
services off:

```rust
// build.rs
fn main() {
    giong_grpc_build::Config::new()
        .services(false)
        .compile(&["proto/events.proto"], &["proto"])
        .unwrap();
}
```

Supported: proto2 and proto3 (`optional`, oneofs, maps, groups, packed and unpacked
repeated fields, closed and open enums, `[default = ...]`, messages that contain
themselves), and services with methods of all four kinds. Not yet: editions and
extensions.

The minimum Rust version is 1.80, as for giong (protox 0.8 is the newest that builds on it).
