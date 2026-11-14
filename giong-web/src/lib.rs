//! # giong-web
//!
//! Routing, middleware and JSON for HTTP services on the [giong]
//! thread-per-core runtime, on top of [giong-http](giong_http).
//!
//! ```no_run
//! use std::time::Duration;
//! use giong::net::TcpListener;
//! use giong_web::middleware::{Compression, Logger, RequestId, Timeout};
//! use giong_web::{Error, Req, Response, Router, Server, Sink};
//!
//! struct State {
//!     greeting: &'static str,
//! }
//!
//! async fn hello(req: Req<State>) -> Result<Response, Error> {
//!     let name: String = req.param("name")?;
//!     Ok(Response::text(format!("{}, {name}!", req.state().greeting)))
//! }
//!
//! fn main() -> std::io::Result<()> {
//!     giong::Builder::new()
//!         .shutdown_on_signals(true)
//!         .shutdown_grace(Duration::from_secs(10))
//!         .build()?
//!         .run(|_worker| async {
//!             // Every worker builds its own app: no locks, no `Send`.
//!             let app = Router::new()
//!                 .get("/hello/{name}", hello)
//!                 .layer(RequestId::new())
//!                 .layer(Logger::new(Sink::Stdout))
//!                 .layer(Compression::new())
//!                 .layer(Timeout::new(Duration::from_secs(10)))
//!                 .build(State { greeting: "Hello" });
//!             let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
//!             Server::new().serve(listener, app).await;
//!         })
//! }
//! ```
//!
//! ## One way to write a handler
//!
//! A handler is an `async fn(Req<S>) -> Result<Response, Error>`. It reads
//! the request through [`Req`]: [`param`](Req::param) for path parameters,
//! [`query_param`](Req::query_param) for the query string, [`Req::json`] for
//! a JSON body (feature `serde`), [`state`](Req::state) for the router's
//! state. It answers with a [`Response`], or [`json`] for a JSON one, which
//! serializes a value ([`Response::json`] takes JSON already written). It uses
//! `?` on any error: what the client should know becomes an [`Error`] with a
//! status, a stable code and a detail; anything else becomes a `500` whose
//! cause goes to the log and never to the client.
//!
//! A handler or a middleware that panics is answered the same way: a `500`
//! (`GIONG-E1210`) whose logged cause is the panic's message, with the
//! request id and the log line of any other error. One request fails, not
//! the connection it came on (nor, over HTTP/2, the other requests on that
//! connection). A program built with `panic = "abort"` still stops at the
//! first panic.
//!
//! ## Errors that explain themselves
//!
//! Every error response is a JSON problem-details body with a stable `code`.
//! Those giong-web makes itself carry giong's codes, each documented with
//! its cause and its fix: `GIONG-E1203` for no route, `GIONG-E1205` for a
//! parameter that does not parse, `GIONG-E1207` for a JSON body that does not
//! fit, and so on. Mistakes in the program itself, such as two routes for the
//! same path, panic at startup with a code and the line that added them.
//!
//! ## Middleware
//!
//! [`middleware`] has [`RequestId`](middleware::RequestId),
//! [`Logger`](middleware::Logger) (JSON lines, written off the worker),
//! [`Cors`](middleware::Cors), [`Compression`](middleware::Compression)
//! (gzip) and [`Timeout`](middleware::Timeout); with the `tracing` feature,
//! `Trace` (a `tracing` span per request). Writing one is implementing
//! [`Middleware`], one `async fn`.
//!
//! ## Streaming
//!
//! The routes of a router with [`Router::stream_bodies`] read their request
//! body as it arrives, with [`Req::body_stream`], instead of receiving it in
//! full first. Any handler can write its response body as it is produced,
//! with [`Body::channel`]: downloads, exports, server-sent events.
//!
//! ## Testing without a network
//!
//! [`App::handle`] answers a [`Request`] made with
//! [`Request::from_bytes`], so routes and middleware can be tested with
//! plain unit tests.
//!
//! ## Features
//!
//! * `serde`: [`Req::json`], [`Req::query`] and [`json`], through serde and
//!   serde_json.
//! * `tls`: [`Server::serve_tls`], for HTTPS.
//! * `tracing`: the `Trace` middleware, a [`tracing`](https://docs.rs/tracing)
//!   span per request.

#![warn(missing_docs)]

#[cfg(feature = "serde")]
mod de;
mod error;
#[cfg(test)]
mod fuzz_tests;
/// Entry points for the fuzz targets in `giong-web/fuzz`, built only for them
/// (`cargo fuzz` sets `--cfg fuzzing`) and the tests. Not part of the API.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzzing;
mod header;
mod json;
mod log;
pub mod middleware;
mod next;
mod percent;
mod req;
mod route;
mod router;

pub use error::Error;
pub use giong_http::{Body, BodySender, BodyStream, Request, Response, Server};
#[cfg(feature = "serde")]
pub use json::json;
pub use log::{flush as flush_logs, Sink};
pub use next::{Middleware, Next};
pub use req::Req;
pub use router::{App, Router};
