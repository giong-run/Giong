//! # giong-http
//!
//! A small, strict HTTP/1.1 and HTTP/2 server for the [giong]
//! thread-per-core runtime, and (with the `client` feature) an HTTP/1.1 and
//! HTTP/2 client: see `giong_http::client`.
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
//! * HTTP/2 (RFC 9113), chosen through ALPN over TLS and with prior
//!   knowledge over plain TCP, for the same handlers: every stream is a
//!   [`Request`] like any other ([`Request::version`] tells them apart).
//!   Flow control, HPACK, trailers, `Expect: 100-continue` and streamed
//!   bodies both ways; limits against rapid reset, frame floods,
//!   CONTINUATION floods, HPACK bombs and stalled readers, each with its
//!   `GIONG-E` code. See [`Server::http2`].
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
//!   bare line feeds, folded headers or trailers, a URL as the target whose
//!   host is not the `Host` header's) are rejected with `400`.
//! * Responses that cannot be split: a header value with a line break is
//!   refused, by [`Response::header`] with a panic and by
//!   [`Response::try_header`] with an error, for values from the request.
//!
//! ## What it does not do (yet)
//!
//! Compression, routing (see the `giong-web` crate) and server push.
//!
//! ## HTTPS
//!
//! With the `tls` feature, [`Server::serve_tls`] serves the same handlers over
//! TLS, using the `giong-tls` crate (rustls). The server offers `h2` and
//! `http/1.1` through ALPN, so browsers get HTTP/2; with
//! [`Server::http2(false)`](Server::http2) it offers only `http/1.1`.
//!
//! ## Client
//!
//! With the `client` feature, `giong_http::client::Client` sends requests,
//! over HTTP or HTTPS, with a pool of kept-alive connections for each origin,
//! and over HTTP/2 when the server picks it through ALPN (or, when asked,
//! with prior knowledge).

#![warn(missing_docs)]

mod body;
mod chunked;
#[cfg(feature = "client")]
pub mod client;
mod date;
#[cfg(test)]
mod fuzz_tests;
mod h2;
mod heap;
#[cfg(any(test, fuzzing))]
mod invariants;
mod parse;
mod request;
mod response;
mod server;

pub use body::{BodySender, BodyStream, StreamBody};
pub use request::{Request, Version};
pub use response::{reason_phrase, Body, FileBody, Response};
pub use server::{Handler, Server, Transport};

/// Entry points for the fuzz targets in `giong-http/fuzz`. Only built by
/// `cargo fuzz` (`--cfg fuzzing`); not part of the API.
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzzing {
    #[cfg(feature = "client")]
    pub use crate::client::fuzzing::{
        check_h2_client, check_inflate, check_response_head, check_url,
    };
    pub use crate::invariants::{
        check_chunked, check_h2_output, check_head, check_hpack, check_huffman, h2_connection,
        h2_input,
    };
}
