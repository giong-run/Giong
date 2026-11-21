//! # giong-proto
//!
//! Protocol Buffers decoded as views over the receive buffer: strings,
//! bytes, repeated fields and maps borrow from the input, so decoding a
//! message copies nothing and allocates nothing per field. Code is
//! generated from `.proto` files by `giong-grpc-build`, with no `protoc`
//! to install.
//!
//! **Prototype for giong 1.1.** Not published; the API will change.
//!
//! ```ignore
//! // Generated from `message Person { string name = 1; repeated string tags = 2; }`
//! use giong_proto::{Encode, View};
//!
//! let person = PersonView::decode(&buf)?; // checks the whole message
//! let name: &str = person.name();         // borrowed from `buf`
//! for tag in person.tags() { /* &str, also borrowed */ }
//! ```
//!
//! Decoding checks everything protobuf requires (UTF-8 in `string` fields,
//! group nesting, varint and length limits, nesting depth) before it
//! returns, so accessors cannot fail. Errors carry `GIONG-E40xx` codes with
//! a cause and a fix ([`ErrorKind`]).
//!
//! What a view costs instead of allocations: a repeated field is read
//! again from the buffer each time it is iterated, and nested messages
//! inside repeated fields are parsed once to check them and again when the
//! iterator reaches them.

#![warn(missing_docs)]

mod error;
pub mod view;
pub mod wire;

pub use error::{Error, ErrorKind};
pub use view::{Encode, Enum, EnumType, Lazy, Map, MessageOf, MessageType, Repeated, View};
pub mod service;
