//! # giong-mysql
//!
//! A MySQL client and connection pool for the [giong] thread-per-core
//! runtime.
//!
//! ```no_run
//! use giong_mysql::{Client, Config};
//!
//! fn main() -> Result<(), giong_mysql::Error> {
//!     let config = Config::from_url("mysql://app:secret@localhost/shop")?;
//!     giong::block_on(async {
//!         let client = Client::connect(&config).await?;
//!         let rows = client
//!             .query("SELECT id, name FROM products WHERE price < ?", &[&100i32])
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
//!   round trip from then on; parameters and results travel in MySQL's
//!   binary format.
//! * **A pool per worker** ([`Pool`]): no locks, no threads in between;
//!   statements that stand alone share connections, transactions take one.
//! * **TLS** through `giong-tls` (rustls), on by default for remote hosts and
//!   always checking the certificate. **`caching_sha2_password`** (MySQL's
//!   default) with its fast path, and its full path over TLS, a Unix socket,
//!   or RSA with the server's public key; `mysql_native_password` too.
//! * **Transactions that cannot end silently.** When the server ends one
//!   early (a deadlock rolls it back, DDL commits it), later statements and
//!   [`Transaction::commit`] fail instead of running outside it.
//! * **Errors that say what to do**, with the server's error number and
//!   SQLSTATE in [`DbError`].
//! * **Limits everywhere**, all finite and documented on [`Config`]: connect
//!   timeout, statement timeout, result size, statement cache, pool wait.
//!
//! ## What it does not do (yet)
//!
//! `LOAD DATA LOCAL INFILE` (refused, so the server cannot read client
//! files), cursors and streaming rows one at a time, result sets after the
//! first from a `CALL`, compression, MariaDB's own extensions, several hosts
//! with failover, and authentication plugins other than those above (LDAP,
//! Kerberos, PAM).

#![warn(missing_docs)]

mod auth;
mod client;
mod config;
mod connect;
mod error;
#[cfg(test)]
mod fuzz_tests;
mod pool;
mod protocol;
mod row;
pub mod types;

pub use client::{CancelToken, Client, Transaction};
pub use config::{Config, Tls};
pub use error::{DbError, Error, ErrorKind};
pub use pool::{Pool, PoolStats, PooledClient};
pub use row::{Column, Row, RowIndex};
pub use types::{Bits, ColumnType, Encoded, FromSql, ToSql};

/// The TLS connector type, re-exported so that [`Tls::Verify`] can be built
/// without depending on `giong-tls` directly.
pub use giong_tls::TlsConnector;
