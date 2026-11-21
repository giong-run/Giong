//! # giong-grpc
//!
//! gRPC for the [giong] thread-per-core runtime: servers and clients over
//! giong-http's HTTP/2, with messages read as [giong-proto] views that
//! borrow from the bytes received. It interoperates with grpc-go, in both
//! directions, on the test cases of gRPC's interoperability specification.
//!
//! Code is generated from `.proto` files at build time by giong-grpc-build,
//! which parses them in Rust: there is no `protoc` to install.
//!
//! ```ignore
//! // build.rs
//! fn main() {
//!     giong_grpc_build::compile(&["proto/greeter.proto"], &["proto"]).unwrap();
//! }
//! ```
//!
//! For each service of the `.proto`, that gives a trait to implement, a
//! `...Server` that routes calls to it, and a `...Client`:
//!
//! ```ignore
//! mod pb {
//!     include!(concat!(env!("OUT_DIR"), "/giong_proto.rs"));
//! }
//! use giong_grpc::{CallOptions, Channel, Reply, Request, Server, Sink, Status};
//! use pb::hello::v1::greeter::{Greeter, GreeterClient, GreeterServer};
//! use pb::hello::v1::{HelloReply, HelloRequest};
//!
//! struct Hello;
//!
//! impl Greeter for Hello {
//!     // rpc SayHello(HelloRequest) returns (HelloReply);
//!     async fn say_hello(
//!         &self,
//!         request: Request<HelloRequest<'static>>,
//!     ) -> Result<Reply<HelloReply<'static>>, Status> {
//!         let name = request.message()?.name(); // borrows from the request
//!         Reply::new(&HelloReply { message: &format!("Xin chào, {name}!"), ..Default::default() })
//!     }
//!
//!     // rpc SayHelloMany(HelloRequest) returns (stream HelloReply);
//!     async fn say_hello_many(
//!         &self,
//!         request: Request<HelloRequest<'static>>,
//!         mut responses: Sink<HelloReply<'static>>,
//!     ) -> Result<(), Status> {
//!         let message = request.message()?;
//!         for n in 1..=message.count() {
//!             let text = format!("Xin chào, {} ({n})!", message.name());
//!             responses.send(&HelloReply { message: &text, ..Default::default() }).await?;
//!         }
//!         Ok(())
//!     }
//! }
//!
//! // The server: a giong-http handler, one per worker.
//! let grpc = Server::new().add(GreeterServer::new(Hello));
//! giong_http::Server::new().serve(listener, grpc).await;
//!
//! // A client.
//! let greeter = GreeterClient::new(Channel::new("http://127.0.0.1:50051")?);
//! let request = HelloRequest { name: "An", count: 3, ..Default::default() };
//! let reply = greeter.say_hello(&request, CallOptions::new()).await?;
//! println!("{}", reply.message()?.message());
//! let mut replies = greeter.say_hello_many(&request, CallOptions::new()).await?;
//! while let Some(received) = replies.message().await? {
//!     println!("{}", received.message()?.message());
//! }
//! ```
//!
//! `examples/grpc-hello` in the repository is this program, complete.
//!
//! ## The four kinds of method
//!
//! | In the `.proto` | The handler takes | and returns | The client method returns |
//! |---|---|---|---|
//! | `rpc M(Req) returns (Resp)` | [`Request<Req>`] | [`Reply<Resp>`] | [`Reply<Resp>`] |
//! | `rpc M(Req) returns (stream Resp)` | [`Request<Req>`], [`Sink<Resp>`] | `()` | [`ResponseStream<Resp>`] |
//! | `rpc M(stream Req) returns (Resp)` | [`RequestStream<Req>`] | [`Reply<Resp>`] | [`ClientStreaming<Req, Resp>`] |
//! | `rpc M(stream Req) returns (stream Resp)` | [`RequestStream<Req>`], [`Sink<Resp>`] | `()` | ([`RequestSink<Req>`], [`ResponseStream<Resp>`]) |
//!
//! Every handler returns `Result<_, Status>`: an `Err` ends the call with
//! that [`Status`], which the client gets as the `Err` of its call.
//!
//! ## What is bounded, and by what
//!
//! Every limit is finite by default; none has to be set for a server to be
//! safe to expose.
//!
//! * **Each message**: 4 MiB, compressed or not, on both sides
//!   ([`Server::max_message_size`], [`ChannelBuilder::max_message_size`]).
//!   A larger one ends its call with `RESOURCE_EXHAUSTED` before it is
//!   buffered.
//! * **Each call**: its deadline. A client sends its timeout (30 seconds
//!   unless [`CallOptions::timeout`] or [`ChannelBuilder::timeout`] says
//!   otherwise) and enforces it; a server enforces the one it was sent, or
//!   [`Server::default_timeout`] (one hour) when the client sent none. Both
//!   end the call with `DEADLINE_EXCEEDED`.
//! * **A stream's pace**: sending waits for the reader. [`Sink::send`] and
//!   [`RequestSink::send`] return when the connection has taken the
//!   message, so a slow reader slows its writer down and nothing queues
//!   without bound.
//! * **Connections, concurrent calls, header sizes, idle time**: giong-http's
//!   limits ([`giong_http::Server`]).
//!
//! ## Cancellation
//!
//! A call ends early when the client cancels it ([`RequestSink::cancel`],
//! dropping a [`ResponseStream`], dropping the future of a unary call), when
//! its deadline passes, or when its connection closes. On the server the
//! handler's future is then dropped at its next `.await`: it does not run
//! to its end, so what it must release belongs in `Drop`, and a step that
//! must finish whatever happens belongs in a task of its own
//! ([`giong::spawn`]). A handler that panics ends its call with `INTERNAL`
//! and nothing else.
//!
//! ## Errors
//!
//! A call that fails ends with a [`Status`]: a [`Code`] and a message. The
//! statuses giong-grpc raises itself carry a giong error code,
//! `GIONG-E4101` to `GIONG-E4110` ([`Status::error_code`]), and say in
//! their message what happened and how to fix it; statuses from a handler
//! or from another implementation are passed on as they are.
//!
//! ## Health and reflection
//!
//! [`health`] is the standard health checking service, which load
//! balancers and Kubernetes probes ask; [`reflection`] lets `grpcurl` and
//! the like list and call a server's methods without its `.proto` files.
//!
//! ## TLS
//!
//! A server with a certificate is served by `giong_http::Server::serve_tls`
//! and clients reach it with an `https://` address; HTTP/2 is chosen
//! through ALPN. [`ChannelBuilder::tls`] takes a private certificate
//! authority or a client certificate.
//!
//! ## Compression
//!
//! gzip, per message, off by default ([`Server::compress`],
//! [`ChannelBuilder::compress`], and one call or one message at a time:
//! [`CallOptions::compress`], [`Reply::compress`], [`Sink::compress`]). A
//! server refuses a request compressed with anything else with
//! `UNIMPLEMENTED` and the list of what it accepts, as the protocol
//! specifies.
//!
//! ## Not there
//!
//! Client-side load balancing and name resolution beyond DNS, retries and
//! hedging (a failed call is returned, not sent again), gRPC-Web, and the
//! `google.rpc.Status` details message (the bytes are available:
//! [`Status::details`]).
//!
//! [giong]: https://docs.rs/giong
//! [giong-proto]: https://docs.rs/giong-proto
#![warn(missing_docs)]

// The code giong-grpc-build generates names this crate by its name.
extern crate self as giong_grpc;

pub mod client;
mod codec;
pub mod health;
mod message;
mod metadata;
pub mod reflection;
pub mod server;
mod status;

pub use client::{
    CallOptions, Channel, ChannelBuilder, ClientStreaming, RequestSink, ResponseStream,
};
pub use message::{Received, Reply};
pub use metadata::Metadata;
pub use server::{Request, RequestStream, Server, Service, Sink};
pub use status::{Code, Status};

/// Entry points for the fuzz targets in `giong-grpc/fuzz`. Only built by
/// `cargo fuzz` (`--cfg fuzzing`); not part of the API.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing;
