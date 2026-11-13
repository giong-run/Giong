# giong-postgres

A PostgreSQL client and connection pool for the [giong](../README.md) runtime. It speaks the wire protocol itself (no libpq), encrypts with [`giong-tls`](../giong-tls/README.md), and depends on nothing from the Tokio ecosystem.

```toml
[dependencies]
giong = "0.3"
giong-postgres = "0.3"
```

```rust
use giong_postgres::{Client, Config};

fn main() -> Result<(), giong_postgres::Error> {
    let config = Config::from_url("postgres://app:secret@localhost/shop")?;
    giong::block_on(async {
        let client = Client::connect(&config).await?;
        let rows = client
            .query("SELECT id, name FROM products WHERE price < $1", &[&100i32])
            .await?;
        for row in &rows {
            let id: i64 = row.get("id")?;
            let name: &str = row.get("name")?;
            println!("{id} {name}");
        }
        Ok(())
    })
}
```

In a service, make one `Pool` per worker. Nothing is shared between threads, so nothing is locked:

```rust
use giong_postgres::{Config, Pool};

fn main() -> std::io::Result<()> {
    let config = Config::from_url("postgres://app:secret@db.internal/shop")?;
    giong::Builder::new().build()?.run(move |_worker| {
        let pool = Pool::new(config.clone(), 4);
        async move {
            // ... serve requests, each one calling pool.query(..) ...
            let row = pool.query_one("SELECT count(*) FROM orders", &[]).await.unwrap();
            let n: i64 = row.get(0).unwrap();
            println!("{n} orders");
        }
    })
}
```

## What it does

- **Pipelining across tasks.** Tasks that share a connection send their statements together, and one round trip answers all of them. A few connections serve hundreds of concurrent tasks, none of them waiting for a free connection.
- **Prepared statements, kept.** Every statement is prepared the first time a connection runs it and kept in a bounded cache (256 per connection by default), so from then on it takes one round trip. When the schema changes under a cached statement, the cache entry is dropped and the next call prepares it again.
- **Binary results for the common types** (`boolean`, `bytea`, the integers, `oid`, the floats, `timestamptz`), and text for the rest, so any column can still be read as a `String`.
- **A pool per worker.** Statements that stand alone share connections; `pool.get()` takes one connection for a transaction or session settings, and rolls back a transaction left open when it comes back. The pool steers new statements away from a connection whose oldest statement has been running for more than 5 ms, so a slow query does not hold up the fast ones behind it.
- **TLS on by default for remote hosts, always verified.** SCRAM-SHA-256, MD5, and clear-text passwords only over TLS or to this machine.
- **Errors that say what to do.** Every error has an `ErrorKind`; errors from the server carry the SQLSTATE code and every field the server sent (`detail`, `hint`, `constraint`...).
- **Finite limits everywhere.** Connecting, running a statement, the size of a result, the statement cache and waiting for the pool all have a limit by default, so nothing waits forever and nothing grows without bound.

## Connecting

```rust
use std::time::Duration;
use giong_postgres::Config;

let config = Config::from_url("postgres://app:secret@db.internal:5432/shop?sslmode=verify-full")?;

let same_place = Config::new()
    .host("db.internal")
    .user("app")
    .password("secret")
    .dbname("shop")
    .statement_timeout(Duration::from_secs(5));
```

A URL is `postgres://user:password@host:port/dbname?param=value` (`postgresql://` too). The host can be an IPv6 address in brackets, or, as with libpq, empty with the socket directory in a `host` parameter: `postgres:///shop?host=/var/run/postgresql`. The parameters understood are `sslmode`, `sslrootcert`, `host`, `port`, `user`, `password`, `dbname`, `application_name` and `connect_timeout` (seconds). Any other parameter is an error, not silently ignored.

**TLS.** Unless `Config::tls` or `sslmode` says otherwise:

| Where the server is | What happens |
|---|---|
| A Unix socket (a host starting with `/`) | No TLS. |
| A loopback address (`localhost`, `127.0.0.0/8`, `::1`) | No TLS. |
| Anywhere else | TLS, with the certificate checked against the certificate authorities installed on the machine and against the host name. |

`sslmode=disable` turns TLS off. `require`, `verify-ca` and `verify-full` all check the certificate and the host name, which is stricter than libpq (libpq checks nothing for `require`). `prefer` and `allow` are refused, because they fall back to an unencrypted connection without saying so. `sslrootcert=/path/ca.pem` trusts only that authority; in code, use `Tls::Verify(TlsConnector::with_root_pem(..)?)`. When TLS is requested and the server does not support it, connecting fails.

A clear-text password is never sent over an unencrypted connection to another machine: the connection fails with `ErrorKind::Auth` instead.

## API

| | |
|---|---|
| `Client::connect(&config)` | Opens a connection: TCP or Unix socket, TLS, authentication, all within `connect_timeout`. |
| `client.query(sql, &[&a, &b])` | All the rows, as `Vec<Row>`. Parameters are `$1`, `$2`... |
| `client.query_one(..)`, `query_opt(..)` | Exactly one row (`ErrorKind::RowCount` otherwise); at most one. |
| `client.execute(..)` | The number of rows affected. |
| `client.batch_execute(sql)` | Several statements separated by `;`, through the simple protocol, with no parameters and no results. For migrations and setup. |
| `client.transaction()` | Starts a transaction and borrows the client mutably until it ends. `tx.commit()` or `tx.rollback()`; dropping the transaction rolls it back. A `COMMIT` that the server turned into a rollback (because of an earlier error) is an error, not a success. |
| `client.cancel_token()` | A `CancelToken` whose `cancel()` asks the server, over a separate connection, to stop the statement running now. |
| `client.parameter(name)` | A setting the server reported, such as `server_version` or `TimeZone`. |
| `client.is_closed()`, `client.in_flight()` | Whether the connection is closed; how many requests are waiting for their answer. |
| `row.get::<T>(column)` | A column by position or name, converted to `T` (see [Types](#types)). Borrowed types (`&str`, `&[u8]`) borrow from the row. |
| `row.is_null(column)`, `row.columns()`, `row.len()` | Whether a value is `NULL`; the columns' names and types; how many there are. |
| `Pool::new(config, max_size)` | A pool for the current worker. Connections are opened when needed, up to `max_size`. Size it to the database server's cores, shared among the workers (see [Performance](#performance)). |
| `.wait_timeout(duration)` | How long `get()` and the statement methods wait when every connection is taken, 5 s by default. |
| `pool.query(..)` and the other statement methods | Run on the least busy connection, pipelined with other tasks' statements. |
| `pool.get()` | A `PooledClient` for this task alone, which derefs to `Client`. For transactions, session settings and statements known to be slow. |
| `pool.stats()` | Connections open, taken, being opened; requests in flight; tasks waiting; and counters for connections opened, connect errors and wait timeouts. |

A `Client` belongs to the worker that opened it (it is not `Send`, like everything in giong). Its methods take `&self`, so tasks share one by cloning an `Rc` or by holding a reference, and their statements are pipelined.

## Types

| Rust | PostgreSQL |
|---|---|
| `bool` | `boolean` |
| `i16`, `i32`, `i64` | `smallint`, `integer`, `bigint` (any of them, checked for range); `oid` |
| `f32`, `f64` | `real`, `double precision` |
| `&str`, `String` | `text`, `varchar`, `char(n)`, `name`; as parameters, any type the server can parse from text (`uuid`, `numeric`, `date`, `json`...); as results, any column the rest of this table does not list |
| `&[u8]`, `Vec<u8>` | `bytea` |
| `SystemTime` | `timestamptz` |
| `Option<T>` | `NULL` or a value |

Integers convert between the three integer types when the value fits and fail when it does not, with an error that names the parameter or column, so a conversion never loses anything silently. Implement `ToSql` and `FromSql` for your own types.

## Errors

`Error` is cheap to clone, implements `std::error::Error`, and converts into `io::Error`. `error.kind()` says what went wrong. Each kind has a giong error code, shown in front of the message and linked to its page in [docs/errors](../docs/errors/README.md) (`GIONG-E3003: password authentication failed for user "app". See …`); after `?` into an `io::Error`, `giong::error::code(&err)` finds it:

| `ErrorKind` | Code | Meaning |
|---|---|---|
| `Config` | [E3001](../docs/errors/E3001.md) | The configuration or URL is not valid. |
| `Connect` | [E3002](../docs/errors/E3002.md) | Name resolution, TCP, TLS, or the connect timeout. Also an unknown database (`3D000`). |
| `Auth` | [E3003](../docs/errors/E3003.md) | The server refused the credentials, or asked for a method this crate does not support. |
| `Db` | [E3004](../docs/errors/E3004.md) | The server reported an error for the statement; `error.code()` is the SQLSTATE (`23505` for a unique violation...) and `error.db()` has every field. |
| `Type` | [E3005](../docs/errors/E3005.md) | A value could not be converted between Rust and PostgreSQL. The message names the parameter or column and suggests the Rust type to use. |
| `RowCount` | [E3006](../docs/errors/E3006.md) | `query_one` found no row, or more than one. |
| `TooLarge` | [E3007](../docs/errors/E3007.md) | The result was larger than `max_result_size`. The connection stays usable. |
| `Closed` | [E3008](../docs/errors/E3008.md) | The connection is closed. A `Pool` replaces it by itself. |
| `PoolTimeout` | [E3009](../docs/errors/E3009.md) | No pooled connection became free within `wait_timeout`. |
| `Protocol` | [E3010](../docs/errors/E3010.md) | The server sent something this crate cannot understand. The connection is closed. |

An error from the server affects only its own statement: the statements that other tasks pipelined on the same connection get their own results.

## Limits

| Setting | Default | What happens at the limit |
|---|---|---|
| `Config::connect_timeout` | 10 s | `ErrorKind::Connect`, "connect timeout". |
| `Config::statement_timeout` | 30 s | Sent as the session's `statement_timeout`, so the server cancels exactly the statement that ran too long (SQLSTATE `57014`). |
| `Config::max_result_size` | 64 MiB of row data | `ErrorKind::TooLarge`; the rest of the result is read and thrown away. |
| `Config::statement_cache_capacity` | 256 per connection | The least recently used statement is closed. 0 prepares and closes every statement each time, for poolers such as PgBouncer in transaction mode. |
| `Pool::wait_timeout` | 5 s | `ErrorKind::PoolTimeout`. |

## Performance

Measured with `bench/postgres.sh` against tokio-postgres 0.7.18 with deadpool-postgres 0.14.2 (Tokio 1.54, current-thread runtime). Each client runs on one core; the same PostgreSQL 16 server runs on the other three cores of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), reached over TCP on 127.0.0.1. 64 tasks issue statements back to back: `point` reads one row by primary key, `list` reads 100 rows, and every column of every row is decoded (`i64`, two `&str`, `SystemTime`). The figures are medians of 5 rounds of 5 s (3 rounds for the other pool sizes), with the range in brackets.

A pool, on one client core:

| Client | Connections | `point` statements/s | p99 | `list` statements/s | p99 |
|---|---|---|---|---|---|
| **giong-postgres** | 3 | **240.7k** (241–249k) | **0.52 ms** | **54.8k** (54–57k) | **2.0 ms** |
| giong-postgres | 4 | 243.9k (211–265k) | 4.2 ms | 53.9k (46–63k) | 5.3 ms |
| tokio-postgres + deadpool | 4 | 53.1k (51–55k) | 1.9 ms | 21.7k (21–25k) | 4.4 ms |
| tokio-postgres + deadpool | 64, its best | 93.8k (91–97k) | 1.5 ms | 28.6k (27–29k) | 4.1 ms |

- **Against deadpool at its best: 2.6× the point lookups and 1.9× the 100-row reads, at a third to a half of the p99.** deadpool's client core is the limit (98–99% busy): every statement is a pool checkout, a round trip and a wake-up. giong-postgres sends the statements of all 64 tasks in a few writes and reads their answers in a few reads.
- **Size the pool to the database's cores.** With more connections than the server has cores (4 on 3 here), one backend at a time waits for a core, and the statements pipelined on its connection, about 16 of them, wait with it: the same throughput, but a p99 of 4 ms. 8 connections give 208.6k with the same p99; 2 connections leave a server core idle, 172.9k. With several workers, the connections of all their pools add up. Statements that wait on disk or on locks need more connections than this.
- **One connection shared by every task** (both libraries pipeline): the single backend process is the limit, one server core at 100%, and the two are within the spread of the rounds of each other. giong-postgres's medians are 8–11% lower (point 47.6k against 53.6k, list 15.7k against 17.0k) and it uses less of its core (46% against 53%, 42% against 61%).

Not measured: a server on another machine (network latency, where pipelining should matter more), writes, more than one client core, PostgreSQL versions other than 16. Results are in `bench/results/postgres/`; pool sizes other than 4 in `pool-size/`.

## Not there yet

`COPY`, `LISTEN`/`NOTIFY`, streaming a result one row at a time (a result is collected in full, up to `max_result_size`), arrays, `numeric`, `uuid`, `date` and the other types not listed above in binary form (read them as strings), channel binding (`SCRAM-SHA-256-PLUS`), several hosts with failover, and SASLprep normalization of non-ASCII passwords. The crate has had no independent security audit.

## Tests

`cargo test -p giong-postgres`: 37 tests against a real PostgreSQL server that the test binary starts for itself (a throwaway cluster from `initdb`, with users for SCRAM, MD5, clear text and TLS only, listening on a Unix socket, 127.0.0.1 and 127.0.0.2): every supported type in both directions; conversion errors; server errors and their fields; pipelining across tasks, and a failing statement in the middle of a pipeline; the statement cache, its bound, and preparing again after a schema change; the result-size limit; the statement timeout; cancel requests; a query dropped mid-flight; a connection the server terminates; transactions committed, rolled back and dropped; no task or connection left behind after a client is dropped; TLS against the right and the wrong authority and host; each authentication method with the right and the wrong password; a server that answers the TLS request with extra bytes; a clear-text password never sent unencrypted; the connect timeout; and the pool (spreading statements, exclusive use and the rollback on return, waiting, replacing broken connections, connect errors, a slow statement not holding up the others). 29 unit tests: the message encoder and parser, SCRAM against the RFC 7677 example, MD5 against RFC 1321, URLs, type conversions, and 42,000 random inputs for the message parser, the URL parser and SCRAM. 7 documentation examples.

The tests find PostgreSQL through `GIONG_PG_BIN`, `/usr/lib/postgresql/*/bin` or `PATH`, and skip when it is not installed, unless `GIONG_PG_REQUIRED=1` (CI sets it). Run as root, they start the server as the `postgres` user. They also pass under ThreadSanitizer and on the fallback io_uring setups (`scripts/check.sh tsan` and `fallback`).
