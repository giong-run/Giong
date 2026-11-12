//! # giong-postgres
//!
//! A PostgreSQL client and connection pool for the [giong] thread-per-core
//! runtime.
//!
//! ```no_run
//! use giong_postgres::{Client, Config};
//!
//! fn main() -> Result<(), giong_postgres::Error> {
//!     let config = Config::from_url("postgres://app:secret@localhost/shop")?;
//!     giong::block_on(async {
//!         let client = Client::connect(&config).await?;
//!         let rows = client
//!             .query("SELECT id, name FROM products WHERE price < $1", &[&100i32])
//!             .await?;
//!         for row in &rows {
//!             let id: i64 = row.get("id")?;
//!             let name: &str = row.get("name")?;
//!             println!("{id} {name}");
//!         }
//!         Ok(())
//!     })
//! }
//! ```
//!
//! ## What it does
//!
//! * **Pipelining across tasks.** Tasks that share a connection send their
//!   statements together, and one round trip answers all of them.
//! * **Prepared statements, kept.** Every statement is prepared the first
//!   time a connection runs it and kept in a bounded cache, so it takes one
//!   round trip from then on; parameters and results travel in binary.
//! * **A pool per worker** ([`Pool`]): no locks, no threads in between;
//!   statements that stand alone share connections, transactions take one.
//! * **TLS** through `giong-tls` (rustls), on by default for remote hosts and
//!   always checking the certificate. **SCRAM-SHA-256** (and MD5, and clear
//!   text only over TLS or to this machine).
//! * **Errors that say what to do**, with the server's SQLSTATE code and
//!   fields in [`DbError`].
//! * **Limits everywhere**, all finite and documented on [`Config`]: connect
//!   timeout, statement timeout (enforced by the server), result size,
//!   statement cache, pool wait.
//!
//! ## What it does not do (yet)
//!
//! `COPY`, `LISTEN`/`NOTIFY`, streaming rows one at a time, arrays and other
//! types beyond those listed in [`types`] (read them as strings), channel
//! binding (`SCRAM-SHA-256-PLUS`), several hosts with failover, and SASLprep
//! normalization of non-ASCII passwords.

#![warn(missing_docs)]

mod auth;
mod client;
mod config;
mod connect;
mod error;
#[cfg(test)]
mod fuzz_tests;
/// Entry points for the fuzz targets in `giong-postgres/fuzz`, built only for them
/// (`cargo fuzz` sets `--cfg fuzzing`) and the tests. Not part of the API.
#[cfg(any(test, fuzzing))]
#[doc(hidden)]
pub mod fuzzing;
mod pool;
mod protocol;
mod row;
pub mod types;

pub use client::{CancelToken, Client, Transaction};
pub use config::{Config, Tls};
pub use error::{DbError, Error, ErrorKind};
pub use pool::{Pool, PoolStats, PooledClient};
pub use row::{Column, Row, RowIndex};
pub use types::{FromSql, ToSql, Type};

/// The TLS connector type, re-exported so that [`Tls::Verify`] can be built
/// without depending on `giong-tls` directly.
pub use giong_tls::TlsConnector;
