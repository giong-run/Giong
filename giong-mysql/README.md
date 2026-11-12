# giong-mysql

A MySQL client and connection pool for the [giong](../README.md) runtime. It speaks the client/server protocol itself (no libmysqlclient), encrypts with [`giong-tls`](../giong-tls/README.md), and depends on nothing from the Tokio ecosystem.

```toml
[dependencies]
giong = "0.2"
giong-mysql = "0.2"
```

```rust
use giong_mysql::{Client, Config};

fn main() -> Result<(), giong_mysql::Error> {
    let config = Config::from_url("mysql://app:secret@localhost/shop")?;
    giong::block_on(async {
        let client = Client::connect(&config).await?;
        let rows = client
            .query("SELECT id, name FROM products WHERE price < ?", &[&100i32])
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
use giong_mysql::{Config, Pool};

fn main() -> std::io::Result<()> {
    let config = Config::from_url("mysql://app:secret@db.internal/shop")?;
    giong::Builder::new().build()?.run(move |_worker| {
        let pool = Pool::new(config.clone(), 4);
        async move {
            // ... serve requests, each one calling pool.query(..) ...
            let id = pool
                .insert("INSERT INTO orders (customer) VALUES (?)", &[&42u64])
                .await
                .unwrap();
            println!("order {id}");
        }
    })
}
```

## What it does

- **Pipelining across tasks.** Tasks that share a connection send their statements together, and one round trip answers all of them. A few connections serve hundreds of concurrent tasks, none of them waiting for a free connection. The usual MySQL clients (libmysqlclient, mysql_async, SQLx) have one statement in flight per connection.
- **Prepared statements, kept.** Every statement is prepared the first time a connection runs it and kept in a bounded cache (256 per connection by default), so from then on it takes one round trip, and parameters and rows travel in MySQL's binary format.
- **A pool per worker.** Statements that stand alone share connections; `pool.get()` takes one connection for a transaction or session settings, and rolls back a transaction left open when it comes back. The pool steers new statements away from a connection whose oldest statement has been running for more than 5 ms, so a slow query does not hold up the fast ones behind it.
- **Transactions that cannot end silently.** In MySQL, a deadlock rolls the whole transaction back, and `CREATE`, `ALTER` or `DROP` commits it; the statements after that run on their own, outside any transaction, without a word. Here they fail instead, and so does `commit`.
- **TLS on by default for remote hosts, always verified.** `caching_sha2_password` (MySQL 8's default) and `mysql_native_password`. A password is never sent where it could be read on the way (see [Passwords](#passwords)).
- **Errors that say what to do.** Every error has an `ErrorKind`; errors from the server carry MySQL's error number and SQLSTATE.
- **Finite limits everywhere.** Connecting, running a `SELECT`, the size of a result, the statement cache and waiting for the pool all have a limit by default, so nothing waits forever and nothing grows without bound.

## Connecting

```rust
use std::time::Duration;
use giong_mysql::Config;

let config = Config::from_url("mysql://app:secret@db.internal:3306/shop?ssl-mode=VERIFY_IDENTITY")?;

let same_place = Config::new()
    .host("db.internal")
    .user("app")
    .password("secret")
    .database("shop")
    .statement_timeout(Duration::from_secs(5));
```

A URL is `mysql://user:password@host:port/database?param=value`. The host can be an IPv6 address in brackets, or empty with the server's Unix socket in a `socket` parameter: `mysql://app@/shop?socket=/var/run/mysqld/mysqld.sock` (in code, a host starting with `/` is a socket). The parameters understood are `ssl-mode`, `ssl-ca`, `socket`, `server-public-key-path`, `connect-timeout` (seconds) and `application-name`. Any other parameter is an error, not silently ignored.

Each connection starts with its session time zone set to UTC (`+00:00`), so `TIMESTAMP` columns, `NOW()` and `SystemTime` agree, and with `max_execution_time` set from the statement timeout. `Config::session_variable("sql_mode", "'STRICT_ALL_TABLES'")` sets other session variables when connecting.

**TLS.** Unless `Config::tls` or `ssl-mode` says otherwise:

| Where the server is | What happens |
|---|---|
| A Unix socket | No TLS. |
| A loopback address (`localhost`, `127.0.0.0/8`, `::1`) | No TLS. |
| Anywhere else | TLS, with the certificate checked against the certificate authorities installed on the machine and against the host name. |

`ssl-mode=DISABLED` turns TLS off. `REQUIRED`, `VERIFY_CA` and `VERIFY_IDENTITY` all check the certificate and the host name, which is stricter than MySQL's own client (it checks nothing for `REQUIRED`). `PREFERRED` is refused, because it falls back to an unencrypted connection without saying so. `ssl-ca=/path/ca.pem` trusts only that authority; in code, use `Tls::Verify(TlsConnector::with_root_pem(..)?)`. When TLS is requested and the server does not offer it, connecting fails.

### Passwords

`caching_sha2_password` logs in with a scramble of the password, which reveals nothing, once the server has the user in its cache. The first login after the server starts (or after `FLUSH PRIVILEGES`) needs the password itself. giong-mysql sends it:

| Connection | How the password goes |
|---|---|
| TLS, or a Unix socket | As it is, inside the encrypted or local connection. |
| No TLS, with `Config::server_public_key` (or `server-public-key-path`) | Encrypted with that RSA key. |
| No TLS, to a loopback address | Encrypted with the RSA key the server sends when asked. Nobody can sit in between on this machine's loopback. |
| No TLS, to another machine, no key configured | Not at all: connecting fails with `ErrorKind::Auth`. Asking the server for its key over the network would let anyone in between replace it and read the password. |

`sha256_password` and clear-text plugins are answered only over TLS or a Unix socket. LDAP, Kerberos and PAM plugins are not supported.

## API

| | |
|---|---|
| `Client::connect(&config)` | Opens a connection: TCP or Unix socket, TLS, authentication, the session settings, all within `connect_timeout`. |
| `client.query(sql, &[&a, &b])` | All the rows, as `Vec<Row>`. Parameters are `?`. A stored procedure (`CALL`) gives the rows of its first result. |
| `client.query_one(..)`, `query_opt(..)` | Exactly one row (`ErrorKind::RowCount` otherwise); at most one. |
| `client.execute(..)` | The number of rows inserted, updated (matched, including rows already holding the new values) or deleted. |
| `client.insert(..)` | The `AUTO_INCREMENT` id an `INSERT` generated (of its first row); `ErrorKind::RowCount` if it generated none. |
| `client.batch_execute(sql)` | Several statements separated by `;`, with no parameters and no results, stopping at the first error. For migrations and setup. |
| `client.transaction()` | Starts a transaction and borrows the client mutably until it ends. `tx.commit()` or `tx.rollback()`; dropping the transaction rolls it back. A transaction the server ended early (deadlock, DDL) refuses further statements, and its `commit` fails. |
| `client.cancel_token()` | A `CancelToken` whose `cancel()` runs `KILL QUERY` over a separate connection, stopping the statement running now. |
| `client.server_version()`, `client.connection_id()` | What the server said when connecting; the id is the one `KILL` and `SHOW PROCESSLIST` use. |
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

Parameters are sent as the MySQL type that holds them exactly; the server converts them to the column's type as it would a literal.

| Rust | Parameter | Columns it reads |
|---|---|---|
| `i8` ... `i64`, `u8` ... `u64` | `TINYINT` ... `BIGINT` (`UNSIGNED`) | Any integer column and `YEAR`, when the value fits; a `DECIMAL` that is a whole number |
| `bool` | `TINYINT` 1 or 0 | An integer column holding 0 or 1 |
| `f32`, `f64` | `FLOAT`, `DOUBLE` | `FLOAT` (both), `DOUBLE` (`f64`) |
| `&str`, `String` | A string in `utf8mb4` | `CHAR`, `VARCHAR`, `TEXT`, `ENUM`, `SET`, `JSON`, `DECIMAL`; `String` also `DATE`, `TIME` and `DATETIME` as text |
| `&[u8]`, `Vec<u8>`, `[u8; N]` | A binary string | Every string and blob column, `BIT`, `GEOMETRY` |
| `SystemTime` | `DATETIME(6)` in UTC | `DATETIME`, `TIMESTAMP` |
| `giong_mysql::Bits` | | `BIT` |
| `Option<T>` | `NULL` or a value | `NULL` or a value |

Conversions never lose anything silently: a value that does not fit, a binary column read as text, or a `NULL` read into a type that cannot hold it fails with an error that names the column and the types that would work. Pass a `DECIMAL` as a string (`"19.99"`) to keep it exact. Implement `ToSql` and `FromSql` for your own types.

## Errors

`Error` is cheap to clone, implements `std::error::Error`, and converts into `io::Error`. `error.kind()` says what went wrong. Each kind has a giong error code, shown in front of the message and linked to its page in [docs/errors](../docs/errors/README.md) (`GIONG-E3104: ERROR 1062 (23000): Duplicate entry '1' for key 'users.PRIMARY'. See …`); after `?` into an `io::Error`, `giong::error::code(&err)` finds it:

| `ErrorKind` | Code | Meaning |
|---|---|---|
| `Config` | [E3101](../docs/errors/E3101.md) | The configuration or URL is not valid, or the server refused a session variable. |
| `Connect` | [E3102](../docs/errors/E3102.md) | Name resolution, TCP, TLS, or the connect timeout. Also an unknown database (1049) and a server older than MySQL 8.0. |
| `Auth` | [E3103](../docs/errors/E3103.md) | The server refused the credentials, asked for a plugin this crate does not support, or would have needed the password where it could be read. |
| `Db` | [E3104](../docs/errors/E3104.md) | The server reported an error for the statement; `error.code()` is MySQL's error number (1062 for a duplicate key, 1213 for a deadlock...) and `error.db()` has the SQLSTATE too. |
| `Type` | [E3105](../docs/errors/E3105.md) | A value could not be converted between Rust and MySQL, or the number of parameters is wrong. The message names the parameter or column and suggests the Rust type to use. |
| `RowCount` | [E3106](../docs/errors/E3106.md) | `query_one` found no row or several, `query_opt` several, `insert` no id. |
| `TooLarge` | [E3107](../docs/errors/E3107.md) | The result was larger than `max_result_size`. The connection stays usable. |
| `Closed` | [E3108](../docs/errors/E3108.md) | The connection is closed. A `Pool` replaces it by itself. |
| `PoolTimeout` | [E3109](../docs/errors/E3109.md) | No pooled connection became free within `wait_timeout`. |
| `Protocol` | [E3110](../docs/errors/E3110.md) | The server sent something this crate cannot understand. The connection is closed. |

An error from the server affects only its own statement: the statements that other tasks pipelined on the same connection get their own results.

## Limits

| Setting | Default | What happens at the limit |
|---|---|---|
| `Config::connect_timeout` | 10 s | `ErrorKind::Connect`, "connect timeout". |
| `Config::statement_timeout` | 30 s | Sent as the session's `max_execution_time`, so the server stops the `SELECT` that ran too long (error 3024). MySQL applies it to read-only `SELECT`s only; writes are bounded by `innodb_lock_wait_timeout`. |
| `Config::max_result_size` | 64 MiB of row data | `ErrorKind::TooLarge`; the rest of the result is read and thrown away. |
| `Config::statement_cache_capacity` | 256 per connection | The least recently used statement is closed. 0 prepares and closes every statement each time. The server's `max_prepared_stmt_count` (16,382 by default) bounds the total. |
| `Pool::wait_timeout` | 5 s | `ErrorKind::PoolTimeout`. |

Values over 16 MiB are split into packets and joined again, both ways. The server's `max_allowed_packet` (64 MiB by default) limits the size of a statement with its parameters, and of a row.

## Performance

Measured with `bench/mysql.sh` against mysql_async 0.36.2 and SQLx 0.8.6, each with its own pool (Tokio 1.54, current-thread runtime). Each client runs on one core; the same MySQL 8.0.46 server runs on the other three cores of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), reached over TCP on 127.0.0.1. 64 tasks issue statements back to back: `point` reads one row by primary key, `list` reads 100 rows, and every column of every row is decoded (`i64`, two strings, a date and time). The figures are medians of 5 rounds of 5 s (3 rounds for the other pool sizes), with the range in brackets.

A pool, on one client core:

| Client | Connections | `point` statements/s | p99 | `list` statements/s | p99 |
|---|---|---|---|---|---|
| **giong-mysql** | 3 | **138.7k** (124–144k) | **0.91 ms** | **35.1k** (27–36k) | **3.2 ms** |
| giong-mysql | 4 | 138.1k (131–144k) | 4.5 ms | 34.3k (32–37k) | 6.9 ms |
| SQLx | 4 | 19.2k (18–20k) | 4.3 ms | 7.6k (7.0–8.1k) | 11.6 ms |
| SQLx | 64, its best | 29.3k (26–30k) | 3.9 ms | 10.4k (9.0–11.7k) | 9.1 ms |
| mysql_async | 4, its best | 19.0k (18–20k) | 5.5 ms | 8.9k (8.4–9.4k) | 10.6 ms |
| mysql_async, default pool settings | 4 | 12.6k (11–13k) | 8.0 ms | 6.4k (5.8–7.0k) | 13.8 ms |

- **Against the best of the others (SQLx with 64 connections): 4.7× the point lookups and 3.4× the 100-row reads, at a quarter to a third of the p99.** SQLx's client core is the limit (99–100% busy). giong-mysql sends the statements of all 64 tasks in a few writes and reads their answers in a few reads; here the server is its limit (its three cores 94% busy), not the client core (74–89%).
- **mysql_async** is not limited by its core (66–90% busy) and does best with 4 connections: with 16 it does 16.8k point lookups, with 64 13.7k at a p99 of 28 ms. By default its pool resets every connection it takes back (`COM_RESET_CONNECTION`, which also drops the connection's prepared statements); that costs it a third of its throughput (the last row). The other mysql_async rows turn it off.
- **Size the pool to the database's cores.** With more connections than the server has cores (4 on 3 here), one server thread at a time waits for a core, and the statements pipelined on its connection wait with it: the same throughput, but a p99 of 4.5 ms instead of 0.91. 8 connections give 132.3k at a p99 of 8.3 ms; 2 connections leave a server core idle, 83.9k. With several workers, the connections of all their pools add up. Statements that wait on disk or on locks need more connections than this.
- **One connection shared by every task:** 28.9k point lookups and 10.4k 100-row reads, about what SQLx does with 64 connections, using 25–30% of the client core. The single server thread is the limit. Neither of the others can share a connection between tasks.

Not measured: a server on another machine (network latency, where pipelining should matter more), writes, more than one client core, MySQL versions other than 8.0.46. Results are in `bench/results/mysql/`; pool sizes other than 4 in `pool-size/`.

## Not there yet

Streaming a result one row at a time (a result is collected in full, up to `max_result_size`) and cursors, the result sets after the first from a `CALL`, `LOAD DATA LOCAL INFILE` (the server is never sent a file), compression, several hosts with failover, MariaDB's own extensions, and authentication plugins other than those above. Tested against MySQL 8.0 only so far. The crate has had no independent security audit.

## Tests

`cargo test -p giong-mysql`: 44 tests against a real MySQL server that the test binary starts for itself (a throwaway data directory from `mysqld --initialize-insecure`, with a user for each authentication plugin and path and one that requires TLS, listening on a Unix socket, 127.0.0.1 and 127.0.0.2): every supported type in both directions; conversion errors that name the column; server errors and their fields; pipelining across tasks, and a failing statement in the middle of a pipeline; the statement cache and its bound, counted on the server, and a cached statement after a schema change; values of 16 MiB and more, both ways; the result-size limit; the statement timeout; `KILL QUERY` through a cancel token; a query dropped mid-flight; a connection the server kills; a stored procedure with two results; `LOAD DATA LOCAL INFILE` refused; transactions committed, rolled back, dropped, ended by DDL and by a deadlock; no task or connection left behind after a client is dropped; TLS against the right and the wrong authority and host; `caching_sha2_password` over a socket, TCP and TLS, with RSA (the key asked for on loopback, or configured), and from a non-loopback address (through a forwarder on this machine's network address) where only the fast path may log in; `mysql_native_password`; `sha256_password` only over TLS; fake servers that ask for the password over the network, lack TLS, or predate MySQL 8; the connect timeout; and the pool (spreading statements, exclusive use and the rollback on return, waiting, replacing broken connections, connect errors, a slow statement not holding up the others). 30 unit tests: packets and their splitting, the scrambles checked the way the server checks them, RSA-OAEP decrypted by the `openssl` command, URLs, type conversions, and 47,000 random inputs for the packet parsers, the framing, rows of every column type, URLs and key files. 8 documentation examples.

The tests find `mysqld` through `GIONG_MYSQL_BIN` (the directory that holds it), `/usr/sbin` or `PATH`, and the `mysql` client next to it or on `PATH`. They skip when MySQL is not installed, unless `GIONG_MYSQL_REQUIRED=1` (CI sets it). They also pass under ThreadSanitizer and on the fallback io_uring setups (`scripts/check.sh tsan` and `fallback`). They have passed against MySQL 8.0.46; MySQL 8.4 and 9 are not tested yet.
