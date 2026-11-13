//! # giong-http
//!
//! A small, strict HTTP/1.1 server for the [giong] thread-per-core
//! runtime, and (with the `client` feature) an HTTP/1.1 client: see
//! `giong_http::client`.
//!
//! ```no_run
//! use giong::net::TcpListener;
//! use giong_http::{Request, Response, Server};
//!
//! async fn handler(req: Request) -> Response {
//!     match (req.method(), req.path()) {
//!         ("GET", "/health") => Response::text("ok"),
//!         ("POST", "/echo") => Response::new(200).body(req.body().to_vec()),
//!         _ => Response::new(404),
//!     }
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     giong::Builder::new()
//!         .shutdown_on_signals(true)
//!         .shutdown_grace(std::time::Duration::from_secs(10))
//!         .build()?
//!         .run(|_worker| async {
//!             // One listener per worker; the kernel spreads connections.
//!             let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
//!             Server::new().serve(listener, handler).await;
//!         })
//! }
//! ```
//!
//! ## What it does
//!
//! * HTTP/1.1 and HTTP/1.0, keep-alive, pipelining (responses to pipelined
//!   requests are sent in one write).
//! * Request bodies with `Content-Length` or chunked encoding, fully received
//!   before the handler runs; `Expect: 100-continue`.
//! * Bodies that do not fit in memory: a handler can read the request body
//!   as it arrives ([`Handler::stream_body`], [`BodyStream`]) and write the
//!   response body as it is produced ([`Body::channel`], [`BodySender`]),
//!   with backpressure both ways.
//! * Limits on head and body size, timeouts for idle connections, slow
//!   requests and slow readers, and an optional cap on open connections.
//! * The client's address on every request ([`Request::peer_addr`]).
//! * Graceful shutdown: requests in progress are answered, idle connections
//!   close.
//! * Strict parsing. Requests that a proxy in front might read differently
//!   (two lengths, a length plus chunked encoding, whitespace before a colon,
//!   bare line feeds, folded headers) are rejected with `400`.
//!
//! ## What it does not do (yet)
//!
//! HTTP/2, compression, routing (see the `giong-web` crate for the last two).
//!
//! ## HTTPS
//!
//! With the `tls` feature, [`Server::serve_tls`] serves the same handlers over
//! TLS, using the `giong-tls` crate (rustls).
//!
//! ## Client
//!
//! With the `client` feature, `giong_http::client::Client` sends requests,
//! over HTTP or HTTPS, with a pool of kept-alive connections for each origin.

#![warn(missing_docs)]

mod body;
mod chunked;
#[cfg(feature = "client")]
pub mod client;
mod date;
#[cfg(test)]
mod fuzz_tests;
mod heap;
#[cfg(any(test, fuzzing))]
mod invariants;
mod parse;
mod request;
mod response;
mod server;

pub use body::{BodySender, BodyStream, StreamBody};
pub use request::Request;
pub use response::{reason_phrase, Body, Response};
pub use server::{Handler, Server, Transport};

/// Entry points for the fuzz targets in `giong-http/fuzz`. Only built by
/// `cargo fuzz` (`--cfg fuzzing`); not part of the API.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    #[cfg(feature = "client")]
    pub use crate::client::fuzzing::{check_inflate, check_response_head, check_url};
    pub use crate::invariants::{check_chunked, check_head};
}
