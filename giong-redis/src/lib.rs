//! # giong-redis
//!
//! A Redis client for the [giong] thread-per-core runtime: one connection
//! carries the commands of many tasks at once, with pipelines,
//! transactions, Pub/Sub and a pool per worker.
//!
//! ```no_run
//! use giong_redis::{cmd, Client, Config};
//!
//! fn main() -> Result<(), giong_redis::Error> {
//!     let config = Config::from_url("redis://localhost:6379")?;
//!     giong::block_on(async {
//!         let client = Client::connect(&config).await?;
//!         client.set_ex("session:42", "data", std::time::Duration::from_secs(3600)).await?;
//!         let session: Option<String> = client.get("session:42").await?;
//!         let top: Vec<String> = client
//!             .query(&cmd("ZRANGE").arg("leaders").arg(0).arg(9).arg("REV"))
//!             .await?;
//!         println!("{session:?} {top:?}");
//!         Ok(())
//!     })
//! }
//! ```
//!
//! ## What it does
//!
//! * **Pipelining across tasks.** Tasks that share a [`Client`] send their
//!   commands together, and one round trip answers all of them, with no
//!   locks and no task in between.
//! * **Any command**, built with [`cmd`] and read as the Rust type asked
//!   for ([`FromValue`]): `Option<String>`, `i64`, `Vec<(String, f64)>`,
//!   `HashMap<String, String>`... Methods for the everyday ones (`get`,
//!   `set`, `set_ex`, `incr`, `hgetall`...).
//! * **Pipelines and transactions** ([`Pipeline`], made
//!   [atomic](Pipeline::atomic) with `MULTI`/`EXEC`), and optimistic
//!   transactions with `WATCH` ([`Client::watch`]).
//! * **Commands that would break sharing are refused** before they are
//!   sent, with what to use instead: `MULTI`, `SELECT`, `SUBSCRIBE`, and
//!   blocking commands outside [`Client::query_blocking`].
//! * **Pub/Sub** ([`PubSub`]), with patterns.
//! * **A pool per worker** ([`Pool`]): shared connections for most
//!   commands, a connection to itself for a task that needs one.
//! * **RESP3** (Redis 6 and later, Valkey), ACL users, TLS through
//!   `giong-tls` (rustls), Unix sockets.
//! * **Limits everywhere**, all finite and documented on [`Config`]:
//!   connect timeout, response timeout, reply size, pool wait. A reply over
//!   the limit is thrown away as it arrives and the connection stays
//!   usable.
//! * **Errors that say what to do**, with the server's error code
//!   (`WRONGTYPE`, `NOSCRIPT`...) in [`Error::server_code`].
//!
//! ## What it does not do (yet)
//!
//! Redis Cluster and Sentinel, client-side caching (`CLIENT TRACKING`),
//! sharded Pub/Sub (`SSUBSCRIBE`), RESP2-only servers (Redis 5 and older),
//! and streaming a large reply in pieces.

#![warn(missing_docs)]

mod client;
mod cmd;
mod commands;
mod config;
mod connect;
mod error;
#[cfg(test)]
mod fuzz_tests;
mod pool;
mod pubsub;
mod resp;
mod value;

pub use client::{Client, Watch};
pub use cmd::{cmd, Cmd, Pipeline};
pub use config::{Config, Tls};
pub use error::{Error, ErrorKind, ServerError};
pub use pool::{Pool, PoolStats, PooledClient};
pub use pubsub::{Message, PubSub};
pub use value::{FromValue, ToArg, Value};

/// The TLS connector type, re-exported so that [`Tls::Verify`] can be built
/// without depending on `giong-tls` directly.
pub use giong_tls::TlsConnector;
