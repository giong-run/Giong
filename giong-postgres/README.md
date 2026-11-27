# giong-postgres

A PostgreSQL client and connection pool for the [giong](../README.md) runtime. It speaks the wire protocol itself (no libpq), encrypts with [`giong-tls`](../giong-tls/README.md), and depends on nothing from the Tokio ecosystem.

```toml
[dependencies]
giong = "1.5.5"
giong-postgres = "1.5.5"
```

```rust,no_run
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

```rust,no_run
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

- **Pipelining across tasks.** Tasks that share a connection send their statements together, and one round trip answers all of them. A few connections serve hundreds of concurrent tasks without each task waiting for a connection of its own.
- **Prepared statements, kept.** Every statement is prepared the first time a connection runs it and kept in a bounded cache (256 per connection by default), so from then on it takes one round trip. When the schema changes under a cached statement (`ALTER TABLE`, a view replaced), the statement is prepared again and run once more, so the caller sees no error; inside a transaction the server has already aborted the transaction, so that one call fails with the server's error and the next one is prepared again. Stale statements are recognised by SQLSTATE and the server's routine name, so this works whatever language the server writes its messages in.
- **Binary results for the common types** (`boolean`, `bytea`, the integers, `oid`, the floats, `timestamptz`), and text for the rest. A column of a text-format type (`uuid`, `numeric`, `date`, `timestamp`, `jsonb`, arrays, `interval`, `inet` and the like) can be read as a `String`; a binary one is read as its Rust type in the [table](#types) below, or cast in SQL (`created_at::text`).
- **A pool per worker.** Statements that stand alone share connections; `pool.get()` takes one connection for a transaction, session settings or a slow statement. When it comes back, a transaction left open is rolled back and the session is reset (settings, temporary tables, advisory locks, `LISTEN`, cursors), so nothing one task did reaches the next. Tasks waiting in `get()` are served in the order they came, even while shared statements keep every connection busy. `get()` can take every connection of the pool: while all of them are held, the pool's own `query` waits too, and fails with `GIONG-E3009` when the wait runs out, so size a pool for the transactions that run at once and one connection more. Transactions and session changes never run on the shared connections: the pool's own methods refuse `BEGIN`, `SET`, `LISTEN`, `PREPARE`, temporary tables and the like before sending them, and a batch that leaves a transaction open closes its connection rather than let the statements of other tasks run inside it. The pool steers new statements away from a connection whose oldest statement has been running for more than 5 ms, so a slow query does not hold up the fast ones behind it. Connections are closed after 10 minutes unused and replaced after 30 minutes, or as soon as their server turns out to be a standby after a failover.
- **TLS on by default for remote hosts, always verified.** SCRAM-SHA-256, MD5, and clear-text passwords only over TLS or to this machine. `require_auth` limits the methods the server may ask for. SCRAM's key derivation runs on a thread of its own, once per password and salt, so connecting does not stall the other tasks of the worker.
- **Errors that say what to do.** Every error has an `ErrorKind`; errors from the server carry the SQLSTATE code and every field the server sent (`detail`, `hint`, `constraint`...).
- **Finite limits everywhere.** Connecting, running a statement, the size of a result, the statement cache and waiting for the pool all have a limit by default, so nothing waits forever and nothing grows without bound.

## Connecting

```rust
use std::time::Duration;
use giong_postgres::{Config, Error};

fn configs() -> Result<(), Error> {
    let config = Config::from_url("postgres://app:secret@db.internal:5432/shop?sslmode=verify-full")?;

    let same_place = Config::new()
        .host("db.internal")
        .user("app")
        .password("secret")
        .database("shop")
        .statement_timeout(Duration::from_secs(5));
    let _ = (config, same_place);
    Ok(())
}
```

A URL is `postgres://user:password@host:port/dbname?param=value` (`postgresql://` too). The host can be an IPv6 address in brackets, or, as with libpq, empty with the socket directory in a `host` parameter: `postgres:///shop?host=/var/run/postgresql`. The parameters understood are `sslmode`, `sslrootcert`, `host`, `port`, `user`, `password`, `dbname`, `application_name`, `connect_timeout` (seconds) and `require_auth`. Any other parameter is an error, not silently ignored.

**TLS.** Unless `Config::tls` or `sslmode` says otherwise:

| Where the server is | What happens |
|---|---|
| A Unix socket (a host starting with `/`) | No TLS. |
| A loopback address (`localhost`, `127.0.0.0/8`, `::1`) | No TLS. |
| Anywhere else | TLS, with the certificate checked against the certificate authorities installed on the machine and against the host name. |

`sslmode=disable` turns TLS off. `require`, `verify-ca` and `verify-full` all check the certificate and the host name, which is stricter than libpq (libpq checks nothing for `require`). `prefer` and `allow` are refused, because they fall back to an unencrypted connection without saying so. `sslrootcert=/path/ca.pem` trusts only that authority; in code, use `Tls::Verify(TlsConnector::with_root_pem(..)?)`. When TLS is requested and the server does not support it, connecting fails.

A clear-text password is never sent over an unencrypted connection to another machine: the connection fails with `ErrorKind::Auth` instead.

**Authentication.** Without TLS (a Unix socket, a loopback address, `sslmode=disable`), nothing proves the server is the real one, and an impostor could ask for MD5 instead of SCRAM or let the client in without asking. `Config::require_auth(&[AuthMethod::ScramSha256])`, or `require_auth=scram-sha-256` in the URL (libpq's names, comma-separated, without libpq's `!` negations), refuses any other method before anything is sent for the password.

### Behind PgBouncer

In transaction mode, PgBouncer gives each transaction any server connection, so two things need setting up:

- **Prepared statements** need PgBouncer 1.21 or later with `max_prepared_statements` above 0 (it then prepares them on whichever server connection runs them). With it, the statement cache works as usual. Without it, prepared statements cannot work through PgBouncer in transaction mode, whatever `statement_cache_capacity` is.
- **The statement timeout** cannot be sent when connecting: PgBouncer refuses the connection, and neither `track_extra_parameters` nor `ignore_startup_parameters` passes it on to the server (checked with PgBouncer 1.22). Call `Config::statement_timeout(None)` and set the limit on the role: `ALTER ROLE app SET statement_timeout = '30s'`. Connecting without `None` fails with an error that says this.

Both are tested in CI against PgBouncer in transaction mode.

## API

| | |
|---|---|
| `Client::connect(&config)` | Opens a connection: TCP or Unix socket, TLS, authentication, all within `connect_timeout`. |
| `client.query(sql, &[&a, &b])` | All the rows, as `Vec<Row>`. Parameters are `$1`, `$2`... |
| `client.query_one(..)`, `query_opt(..)` | Exactly one row (`ErrorKind::RowCount` otherwise); at most one. |
| `client.execute(..)` | The number of rows affected. |
| `client.batch_execute(sql)` | Several statements separated by `;`, through the simple protocol, with no parameters and no results. For migrations and setup. |
| `client.transaction()` | Starts a transaction and borrows the client mutably until it ends. `tx.commit()` or `tx.rollback()`; dropping the transaction rolls it back. A `COMMIT` that the server turned into a rollback (because of an earlier error) is an error, not a success. |
| `client.cancel_token()` | A `CancelToken` whose `cancel()` asks the server, over a separate connection, to stop the statement running now. A token taken from a `PooledClient` stops working when the connection goes back to the pool, and a connection that goes back while such a request is on its way is closed rather than lent again, so it cannot cancel another task's statement. |
| `client.parameter(name)` | A setting the server reported, such as `server_version` or `TimeZone`. |
| `client.is_closed()`, `client.in_flight()` | Whether the connection is closed; how many requests are waiting for their answer. |
| `row.get::<T>(column)` | A column by position or name, converted to `T` (see [Types](#types)). Borrowed types (`&str`, `&[u8]`) borrow from the row. |
| `row.is_null(column)`, `row.columns()`, `row.len()` | Whether a value is `NULL`; the columns' names and types; how many there are. |
| `Pool::new(config, max_size)` | A pool for the current worker. Connections are opened when needed, up to `max_size`. Size it to the database server's cores, shared among the workers (see [Performance](#performance)). |
| `.wait_timeout(duration)` | How long `get()` and the statement methods wait when every connection is taken, 5 s by default. |
| `.idle_timeout(duration)`, `.max_lifetime(duration)` | Close a connection unused for this long (10 min by default); replace one older than this (30 min, less up to 10% so that connections opened together are not replaced together). |
| `pool.query(..)` and the other statement methods | Run on the least busy connection, pipelined with other tasks' statements. They refuse transaction statements, session changes and `COPY ... FROM STDIN` before sending them (`ErrorKind::Statement`). |
| `Config::batch_reads(true)` | Lets the pool send the `SELECT`s waiting for one shared connection under one `Sync`, up to 16, never waiting for more (see [Batched reads](#batched-reads)). Off by default. |
| `pool.get()` | A `PooledClient` for this task alone, which derefs to `Client`. For transactions, session settings and statements known to be slow. Served in the order asked. When it is dropped, a transaction left open is rolled back and, if anything was sent, the session is reset; settings given to `Config` survive the reset. |
| `pool.stats()` | Connections open, taken, being opened; requests in flight; tasks waiting; and counters for connections opened, connect errors, wait timeouts and connections recycled. |
| `Config::require_auth(&[AuthMethod::..])` | The only authentication methods the server may ask for. |

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
| `Db` | [E3004](../docs/errors/E3004.md) | The server reported an error for the statement; `error.server_code()` is the SQLSTATE (`23505` for a unique violation...) and `error.db()` has every field. A `COMMIT` that the server turned into a rollback is `Db` with code `25P02` and no `db()`. |
| `Type` | [E3005](../docs/errors/E3005.md) | A value could not be converted between Rust and PostgreSQL. The message names the parameter or column and suggests the Rust type to use. |
| `RowCount` | [E3006](../docs/errors/E3006.md) | `query_one` found no row, or more than one. |
| `TooLarge` | [E3007](../docs/errors/E3007.md) | The result was larger than `max_result_size`. The connection stays usable. |
| `Closed` | [E3008](../docs/errors/E3008.md) | The connection is closed. A `Pool` replaces it by itself. Also a cancel token whose pooled connection went back to the pool. |
| `PoolTimeout` | [E3009](../docs/errors/E3009.md) | No pooled connection became free within `wait_timeout`. |
| `Protocol` | [E3010](../docs/errors/E3010.md) | The server sent something this crate cannot understand. The connection is closed. |
| `Statement` | [E3011](../docs/errors/E3011.md) | Refused before sending, on the connections a `Pool` shares: a transaction statement, a statement that changes the session (`SET`, `LISTEN`, `PREPARE`, temporary tables...), or `COPY ... FROM STDIN`. Use `pool.get()`. The check reads the statement, not what a function does: `SELECT set_config('search_path', 'x', false)` is sent, and changes the session for every task that shares the connection. |

`error.is_transient()` is true for errors worth retrying after a pause: connecting, a closed connection, a pool wait timeout, and the server's serialization failure (`40001`), deadlock (`40P01`), lock not available (`55P03`), too many connections (`53300`), connection errors (class `08`), shutdown or startup (`57P01`, `57P02`, `57P03`) and standby (`25006`). Retry a whole transaction, not its failed statement.

An error from the server affects only its own statement: the statements that other tasks pipelined on the same connection get their own results. Two things close the connection instead, and every statement sent on it after them fails with `ErrorKind::Closed`: `COPY ... FROM STDIN` on a `Client` (COPY is not supported yet; the pool refuses it), and a `batch_execute` that leaves a transaction open on a pool's shared connection.

## Limits

| Setting | Default | What happens at the limit |
|---|---|---|
| `Config::connect_timeout` | 10 s | `ErrorKind::Connect`, "connect timeout". |
| `Config::statement_timeout` | 30 s | Sent as the session's `statement_timeout`, so the server cancels exactly the statement that ran too long (SQLSTATE `57014`). From 1 ms to `i32::MAX` ms (24.8 days); outside that, connecting fails with `ErrorKind::Config`. `None` sends nothing and leaves the limit to the server or the role. |
| `Config::max_result_size` | 64 MiB of row data | `ErrorKind::TooLarge`; the rest of the result is read and thrown away. A notice larger than this is skipped; an error message larger than this fails its statement with `TooLarge`. Either way the connection stays usable. |
| `Config::statement_cache_capacity` | 256 per connection | The least recently used statement is closed. 0 prepares and closes every statement each time. |
| `Pool::wait_timeout` | 5 s | `ErrorKind::PoolTimeout`. It bounds the wait for a connection; a task that opens a new connection itself waits up to `connect_timeout` instead. |
| `Pool::idle_timeout` | 10 min | The connection is closed. |
| `Pool::max_lifetime` | 30 min, less up to 10% | The connection is closed once no statement uses it, and a new one is opened when needed. |
| Statements on one pooled connection at a time | 1024 | The statement methods use another connection, or wait for room. A `Client` used directly has no such bound: its callers decide how many tasks share it. |

## Performance

Measured with `bench/postgres.sh` against tokio-postgres 0.7.18 with deadpool-postgres 0.14.2 (Tokio 1.54, current-thread runtime). Each client runs on one core; the same PostgreSQL 16 server runs on the other three cores of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), reached over TCP on 127.0.0.1. 64 tasks issue statements back to back: `point` reads one row by primary key, `list` reads 100 rows, and every column of every row is decoded (`i64`, two `&str`, `SystemTime`). The figures are medians of 5 rounds of 5 s (3 rounds for the other pool sizes), with the range in brackets.

A pool, on one client core:

| Client | Connections | `point` statements/s | p99 | `list` statements/s | p99 |
|---|---|---|---|---|---|
| **giong-postgres** | 3 | **311.4k** (308–322k) | **0.43 ms** | **71.3k** (70–74k) | **2.0 ms** |
| giong-postgres | 4 | 303.8k (277–317k) | 2.9 ms | 72.9k (64–76k) | 4.9 ms |
| tokio-postgres + deadpool | 4 | 55.4k (53–56k) | 1.7 ms | 25.4k (24–26k) | 3.9 ms |
| tokio-postgres + deadpool | 64, its best | 93.8k (91–97k) | 1.5 ms | 28.6k (27–29k) | 4.1 ms |

- **Against deadpool at its best: 3.3× the point lookups and 2.5× the 100-row reads, at a third to a half of the p99.** deadpool's client core is the limit (98–99% busy): every statement is a pool checkout, a round trip and a wake-up. giong-postgres sends the statements of all 64 tasks in a few writes and reads their answers in a few reads.
- **Size the pool to the database's cores.** With more connections than the server has cores (4 on 3 here), one backend at a time waits for a core, and the statements pipelined on its connection, about 16 of them, wait with it: the same throughput, but a p99 of 2.9 ms instead of 0.43. 8 connections give 253.8k at a p99 of 1.4 ms (8.6 ms for 100 rows); 2 connections leave a server core idle, 183.9k. With several workers, the connections of all their pools add up. Statements that wait on disk or on locks need more connections than this.
- **One connection shared by every task** (both libraries pipeline): the single backend process is the limit, one server core at 100%, and here giong-postgres is behind: its medians are 7–9% lower (point 47.7k against 51.4k, list 18.7k against 20.5k; the same with the two run in the other order), though the ranges of the rounds overlap, and it uses less of its core (50% against 52%, 43% against 64%). Both send the same messages, one `Sync` per statement, and the server spends half of its time in `send()`, writing one reply per statement and waking the client for it, and the gap is in that part (a quarter of the server's time with giong-postgres, against slightly less with tokio-postgres, whose busier client needs waking less often). Sending several statements under one `Sync` cuts the server's work, but it runs them in one transaction, so it is only done by a pool with `batch_reads` (below). Use a `Pool`, where the client is the limit and giong-postgres does 5.5 times as much.

### Batched reads

With `Config::batch_reads(true)`, the `SELECT`s that a pool's shared connection has waiting when it writes go to the server under one `Sync`, at most 16, behind a prepared statement that makes their transaction read-only; a statement never waits for others to join it. The server then sends one reply and wakes the client once per batch rather than once per statement, which matters when the server is the busy side. Measured with `bench/postgres.sh` (`batch` mode, 3 rounds of 5 s, 64 tasks, `point`; results in `bench/results/postgres/batch/`):

| Server cores | Connections | without batches | p99 | with batches | p99 |
|---|---|---|---|---|---|
| 1 | 1 | 58.7k (54–61k) | 2.0 ms | **198.3k** (197–204k) | **0.56 ms** |
| 1 | 4 | 50.5k (49–55k) | 12.5 ms | **171.9k** (167–187k) | **0.75 ms** |
| 3 | 4 | 314.2k (273–327k) | 2.5 ms | **370.4k** (318–372k) | **0.39 ms** |

- **3.4 times the point lookups when the database is the limit, and up to 18% more with it on three cores**, where the client core is then no longer full (71% busy against 98%): the server's work per statement is what shrinks.
- **No gain for the 100-row reads** (`list`: 81.6k against 80.5k on three cores, 21.8k against 22.6k on one; the ranges overlap): producing the rows costs far more than the reply's `send()`.
- **The full path of an HTTP request gains little**, because there the HTTP side is the limit (`bench/db-path.sh`, results in `bench/results/db-path/`): with one giong worker and the database on two cores, 82.9k against 86.9k requests a second (ranges 79–96k and 75–93k), the database 84% busy against 93%; with two giong workers and the database on one core (`db-bound/`), 96.9k against 91.7k, p99 3.1 ms against 4.7 ms. Few requests arrive in the same tick for one connection there: 1.7 statements a batch on average, measured from the server's transaction count. One request at a time is unchanged (88–92 µs median).
- **Transaction-local settings.** The statements of a batch share one transaction, so a setting made with `set_config(.., true)` would apply to the other tasks' statements after it in the batch. A `SELECT` whose text calls `set_config` therefore runs alone; one that calls a function that makes such a setting (`set_config(.., true)` or `SET LOCAL` inside it) cannot be seen, so leave `batch_reads` off where row-level security or anything else is set up that way per request.
- What else changes for a statement in a batch is in the documentation of `Config::batch_reads`: in short, a write is refused in the batch (`25006`) and the statement is sent again alone; when a statement or the batch's commit fails, the batch is rolled back and every other statement of it is sent again alone; answers arrive when the batch ends; `now()` is the batch's start.
- MySQL has nothing like it: its protocol has no `Sync`, every statement gets its own reply, and giong-mysql already pipelines them.

Not measured: a server on another machine (network latency, where pipelining should matter more), writes, more than one client core, PostgreSQL versions other than 16. Results are in `bench/results/postgres/`; pool sizes other than 4 in `pool-size/`.

## Not there yet

`COPY`, `LISTEN`/`NOTIFY`, streaming a result one row at a time (a result is collected in full, up to `max_result_size`), arrays, `numeric`, `uuid`, `date` and the other types not listed above in binary form (read them as strings), channel binding (`SCRAM-SHA-256-PLUS`), client certificates in the URL (`sslcert` and `sslkey` are refused; in code, give the connector one with `TlsConnector::client_auth`), several hosts with failover, running through PgBouncer without `max_prepared_statements`, and SASLprep normalization of non-ASCII passwords. The crate has had one independent review before 1.0, whose findings are fixed, but no security audit.

## Tests

`cargo test -p giong-postgres`: 70 tests against a real PostgreSQL server that the test binary starts for itself (a throwaway cluster from `initdb`, with users for SCRAM, MD5, clear text and TLS only, listening on a Unix socket, 127.0.0.1 and 127.0.0.2): every supported type in both directions; conversion errors; server errors and their fields; pipelining across tasks, and a failing statement in the middle of a pipeline; the statement cache, its bound, statement names an application took, and schema changes under cached statements, in and out of a transaction; notices and errors larger than the result limit; the result-size limit; the statement timeout, and leaving it to the server; cancel requests; a query dropped mid-flight; a connection the server terminates; transactions committed, rolled back and dropped, and a `COMMIT` turned into a rollback; no task or connection left behind after a client is dropped; TLS against the right and the wrong authority and host; each authentication method with the right and the wrong password, and `require_auth`; SCRAM keys derived once and off the worker; a server that answers the TLS request with extra bytes; a clear-text password never sent unencrypted; the connect timeout; and the pool (spreading statements, exclusive use and the rollback on return, even with `BEGIN` still in flight, the session reset on return and no reset for an unused connection, `get()` served in order while shared statements keep every connection busy, waiting, and waking when a dropped statement frees the connection, cancel tokens that stop working on return, idle and old connections closed, at most 1024 statements per connection at a time, replacing broken connections, connect errors, a slow statement not holding up the others, transaction and session statements refused on shared connections and a batch that leaves a transaction open; batched reads: each statement's own rows, one failing statement among good ones, `SELECT`s that write and lock rows, reads after writes, statements leaving the cache mid-batch, an error at a batch's commit, statements rolled back with their batch and run again, a statement that calls `set_config` kept out of batches (and one hidden in a function shared within its batch), the read-only guard prepared again after a `DEALLOCATE`, and no deadlock with a migration or with another batch taking locks in the opposite order). Two of them run a workload through PgBouncer in transaction mode, with the statement timeout left to the role, when PgBouncer is installed. 45 unit tests: the message encoder and parser, SCRAM against the RFC 7677 example, MD5 against RFC 1321, URLs, type conversions and how their errors name types, the statements the pool refuses and the ones it batches (never one that calls `set_config`), which errors are worth a retry, the old names of renamed methods, 42,000 random inputs for the message parser, the URL parser and SCRAM, and 300 sessions against a fake server answering with mutated replies (also the `session` fuzz target). 8 documentation examples.

The tests find PostgreSQL through `GIONG_PG_BIN`, `/usr/lib/postgresql/*/bin` or `PATH`, and skip when it is not installed, unless `GIONG_PG_REQUIRED=1` (CI sets it); PgBouncer likewise through `GIONG_PGBOUNCER_BIN`, `PATH` or `/usr/sbin`, and `GIONG_PGBOUNCER_REQUIRED=1`. Run as root, they start the server as the `postgres` user. They also pass under ThreadSanitizer and on the fallback io_uring setups (`scripts/check.sh tsan` and `fallback`).
