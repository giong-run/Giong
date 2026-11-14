//! HTTP/2 (RFC 9113): the server side of a connection, and the pieces both
//! sides share (frames, HPACK).
//!
//! A server speaks HTTP/2 on a connection when TLS negotiated `h2` through
//! ALPN, or when a connection without TLS starts with the HTTP/2 preface
//! ("prior knowledge"). Handlers see the same [`Request`](crate::Request)
//! and return the same [`Response`](crate::Response) as over HTTP/1.1.

pub(crate) mod frame;
pub(crate) mod hpack;
mod request;
mod server;
mod wake;

pub(crate) use frame::PREFACE;
pub(crate) use server::serve;
#[cfg(test)]
mod tests;
