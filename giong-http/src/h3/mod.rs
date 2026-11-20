//! HTTP/3 (RFC 9114): what a connection is made of, before any transport.
//! Variable-length integers, frames, settings, stream types and error
//! codes ([`frame`]), and QPACK field compression ([`qpack`]).
//!
//! Behind the `unstable-http3` feature while the QUIC transport is being
//! built: nothing here is reachable from the public API yet.

// Until the server and the client that use them are in.
#![allow(dead_code)]

pub(crate) mod frame;
pub(crate) mod qpack;
#[cfg(test)]
mod tests;
