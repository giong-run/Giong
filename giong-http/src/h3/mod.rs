//! HTTP/3 (RFC 9114): what a connection is made of, before any transport.
//! Variable-length integers, frames, settings, stream types and error
//! codes ([`frame`]), and QPACK field compression ([`qpack`]).
//!
//! The server side of a connection is in [`server`], behind the
//! `unstable-http3` feature: `Server::serve_h3` is its entry point.

// Without the feature (in the unit tests and the fuzz build) only the wire
// format is compiled, and nothing uses it.
#![cfg_attr(not(feature = "unstable-http3"), allow(dead_code))]

pub(crate) mod frame;
pub(crate) mod qpack;
#[cfg(feature = "unstable-http3")]
pub(crate) mod qpack_conn;
#[cfg(feature = "unstable-http3")]
pub(crate) mod server;
#[cfg(all(test, feature = "unstable-http3"))]
mod server_tests;
#[cfg(test)]
mod tests;
