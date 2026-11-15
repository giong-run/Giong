# giong-grpc-build

Generates [giong-proto](../giong-proto) code from `.proto` files at build time. The
`.proto` files are parsed in Rust by [protox](https://docs.rs/protox) (Google's
well-known types included), so there is **no `protoc` to install**: `cargo build` is
enough, on any machine an agent or a CI runner starts with.

> **Prototype for giong 1.1.** Not published, and not on `main` before giong 1.0.0 is
> released.

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
types in module `m`, a Rust enum per protobuf enum, and per service the paths and kinds of
its methods. [giong-proto's README](../giong-proto/README.md) describes the generated API;
[`examples/proto-hello`](../examples/proto-hello) uses it.

Supported: proto2 and proto3 (`optional`, oneofs, maps, groups, packed and unpacked
repeated fields, closed and open enums, `[default = ...]`, messages that contain
themselves). Not yet: editions, extensions, service traits and client stubs (they come
with giong-grpc).

The minimum Rust version is 1.80, as for giong (protox 0.8 is the newest that builds on it).
