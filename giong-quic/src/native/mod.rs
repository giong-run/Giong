//! The native QUIC engine, built for thread-per-core and io_uring. Work in
//! progress: see DESIGN.md. Crypto and the TLS 1.3 handshake stay with
//! rustls (and ring); this module owns packets, frames, loss recovery,
//! congestion control and the connection state machine.

pub mod cid;
pub mod hello;
pub mod packet;
pub mod varint;
