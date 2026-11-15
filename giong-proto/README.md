# giong-proto

Protocol Buffers for [giong](https://github.com/giong-run/Giong): messages decoded as
**views over the receive buffer**. Strings, bytes, repeated fields and maps borrow from
the input, so decoding copies nothing and makes no heap allocation; encoding writes
message structs whose fields borrow too. Code is generated from `.proto` files by
[giong-grpc-build](../giong-grpc-build), which parses them in Rust: no `protoc` to install.

> **Prototype for giong 1.1.** Not published, and not on `main` before giong 1.0.0 is
> released. The API will change. It is the message layer for gRPC on giong's HTTP/2.

## Use

`build.rs`:

```rust
fn main() {
    giong_grpc_build::compile(&["proto/shop.proto"], &["proto"]).unwrap();
}
```

The crate:

```rust
mod pb {
    include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
}
use giong_proto::Encode;
use pb::shop::v1::{LineItem, Order, OrderView};

// Encode: a message struct whose fields borrow. `..Default::default()` keeps the
// code compiling when fields are added to the .proto.
let items = [LineItem { sku: "TEA-001", quantity: 2, ..Default::default() }];
let bytes = Order { id: 1001, customer: "Nguyễn Văn A", items: &items, ..Default::default() }
    .encode_to_vec()?;

// Decode: checks the whole message once, then every accessor is infallible and
// returns data borrowed from `bytes`.
let order = OrderView::decode(&bytes)?;
assert_eq!(order.customer(), "Nguyễn Văn A");
for item in order.items() {
    println!("{} x{}", item.sku(), item.quantity());
}
```

[`examples/proto-hello`](../examples/proto-hello) is a complete program with enums,
oneofs, maps, `optional` fields and a service (`cargo run -p proto-hello`).

## What is generated

For each message `M` in package `a.b` (module `a::b`):

| Item | What it is |
|---|---|
| `MView<'a>` | the decoded message. `MView::decode(&buf)` checks everything protobuf requires, then accessors return `&'a str`, `&'a [u8]`, numbers, `Enum<E>`, nested views, and `&Repeated` / `&Map` for repeated fields and maps, read lazily from the buffer. Singular fields with presence (proto2, proto3 `optional`) have `has_x()`; absent fields return their default (proto2 `[default = ...]` included). |
| `M<'a>` | the message to build and encode: plain public fields (`&'a str`, `&'a [T]`, `Option<&'a Sub>`, `&'a [(K, V)]` for maps). Implements `Encode`. |
| `m::OneofView<'a>`, `m::Oneof<'a>` | an enum per oneof, for each side. Nested messages and enums are in the module `m`. |
| `E` | a Rust enum per protobuf enum; fields hold `Enum<E>`, which keeps numbers the enum does not define (`.known()`, `.raw()`). |
| `service::METHOD` | for each service, a module with the `:path` and kind (unary, client, server or bidi streaming) of every method, for giong-grpc. |

A view's `Encode` writes **the bytes it was decoded from**, unchanged (a view cannot be
modified, so they are its encoding): forwarding a received message is one copy.
`encode_canonical` writes it field by field instead, in Google's order.

## Errors

Decoding and encoding errors carry codes `GIONG-E4001` to `GIONG-E4008`
([`ErrorKind`](src/error.rs)), each with a page in [`docs/errors/`](../docs/errors/README.md):
truncated input, malformed varints, invalid tags, unbalanced groups, non-UTF-8 strings,
nesting deeper than 100, malformed packed fields, messages over 2 GiB.

## How it is checked

- **Google's protobuf conformance suite** (`conformance_test_runner` from protobuf
  v36.2): every binary-format test passes, required and recommended, 1414 of 1414, for
  proto2 and proto3. JSON and text-format tests are skipped: giong-proto reads and writes
  the binary format only. Run it with `giong-proto/conformance/run.sh`.
- **Differential tests against prost** (`giong-proto/conformance`): the same bytes into
  both; when both accept, prost must read giong-proto's canonical re-encoding as the same
  message; when only one accepts, the difference must be one of two documented ones
  (below). A seeded generator runs in the test suite, and the `differential` target in
  `fuzz/` runs the same check under libFuzzer.
- Unit tests for the wire format, a test of the message structs against prost for every
  kind of field, and the example's tests.

Where giong-proto follows Google's libraries and prost does not (the differential test
allows exactly these):

1. A known field arriving with the wrong wire type is kept as an unknown field; prost
   rejects the message (and for map fields skips the check and misreads what follows).
2. A proto2 (closed) enum field holding a number the enum does not define keeps that
   number as an unknown field; prost stores it as the field's value.

Known gaps against Google's libraries: proto2 `string` fields are checked for UTF-8
(Google accepts invalid UTF-8 there unchecked; prost rejects it, as giong-proto does);
editions (`edition = "2023"`) and extensions are not supported; no JSON or text format.

## Speed

`bench/proto.sh` against prost 0.13 with its defaults (as tonic uses it), one core of the
cloud VM, median of 10 rounds; raw data in [`bench/results/proto/`](../bench/results/proto).
"decode+read" decodes and reads every field, the fair comparison since views read
repeated fields lazily:

| Message | Bytes | decode+read prost → giong-proto | ratio | encode prost → giong-proto | ratio | heap allocations per decode, prost / giong-proto |
|---|---:|---|---:|---|---:|---|
| 6 scalars | 34 | 26 ns → 27 ns | 0.97x | 14 ns → 13 ns | 1.08x | 0 / 0 |
| user record (strings, map) | 187 | 462 ns → 186 ns | 2.49x | 72 ns → 51 ns | 1.42x | 17 / 0 |
| list of 100 items | 9211 | 17.3 µs → 11.9 µs | 1.45x | 5.6 µs → 2.9 µs | 1.97x | 607 / 0 |
| 64 KiB bytes field | 65552 | 3.0 µs → 16 ns | 192.26x | 1.5 µs → 1.5 µs | 1.00x | 3 / 0 |
| descriptor.proto set (upb's benchmark input) | 53773 | 198.6 µs → 100.9 µs | 1.97x | not measured (no hand-built message) | – | 3688 / 0 |

What a view costs instead of allocations: nested messages inside repeated fields are
parsed once at decode (to check them) and again when the iterator reaches them, so
messages made mostly of small repeated sub-messages gain less than the others.

## For agents

- One way to read (`XView::decode`, then accessors) and one way to write (the `X` struct,
  then `encode`). Accessors never fail: all checking happens in `decode`.
- Views borrow the input: keep the buffer alive as long as the view. To keep data longer,
  copy what you need (`.to_string()`, `.to_vec()`), or keep the bytes and decode again.
- Match errors on `err.kind()` (`giong_proto::ErrorKind`), not on the message text.
