# giong-mysql

A MySQL and MariaDB client and connection pool for the [giong](../README.md) runtime. It speaks the client/server protocol itself (no libmysqlclient), encrypts with [`giong-tls`](../giong-tls/README.md), and depends on nothing from the Tokio ecosystem.

```toml
[dependencies]
giong = "1.4.0"
giong-mysql = "1.4.0"
```

```rust,no_run
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

```rust,no_run
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
                .unwrap()
                .expect("orders has an AUTO_INCREMENT id");
            println!("order {id}");
        }
    })
}
```

## What it does

- **Pipelining across tasks.** Tasks that share a connection send their statements together, and one round trip answers all of them. A few connections serve hundreds of concurrent tasks, none of them waiting for a free connection. The usual MySQL clients (libmysqlclient, mysql_async, SQLx) have one statement in flight per connection.
- **Prepared statements, kept.** Every statement is prepared the first time a connection runs it and kept in a bounded cache (64 per connection by default), so from then on it takes one round trip, and parameters and rows travel in MySQL's binary format. The server's `max_prepared_stmt_count` bounds the statements of all its clients together; when it is reached anyway (error 1461), the connection closes its statements that are not in use and prepares again, once.
- **A pool per worker.** Statements that stand alone share connections; `pool.get()` takes one connection for a transaction, session settings or a slow statement. A connection comes back clean: if its session may have changed (a `SET`, a temporary table, a lock, a transaction left open), it is reset before anything else runs on it, without waiting for a round trip; one that ran only plain statements keeps its prepared statements. Tasks waiting in `get()` are served in order, and shared statements cannot keep them waiting: a connection is set aside for the oldest and handed over once the statements on it are answered. Transactions and session changes never run on the shared connections: the pool's own methods refuse `BEGIN`, `COMMIT`, `SET`, `LOCK TABLES` and the like, and a batch that leaves a transaction open closes its connection rather than let the statements of other tasks run inside it. The pool steers new statements away from a connection whose oldest statement has been running for more than 5 ms, so a slow query does not hold up the fast ones behind it.
- **Transactions that cannot end silently.** In MySQL, a deadlock rolls the whole transaction back, so does a lock wait timeout when the server has `innodb_rollback_on_timeout` on, and `CREATE`, `ALTER` or `DROP` commits it; the statements after that run on their own, outside any transaction, without a word. Here they fail instead, and so does `commit`. So does a transaction in which a statement was given up (its future dropped by a timeout), since it may have run.
- **TLS on by default for remote hosts, always verified.** `caching_sha2_password` (MySQL 8's default) and `mysql_native_password`. A password is never sent where it could be read on the way (see [Passwords](#passwords)).
- **Errors that say what to do.** Every error has an `ErrorKind`; errors from the server carry MySQL's error number and SQLSTATE.
- **Finite limits everywhere.** Connecting, running a `SELECT`, waiting for a lock, the size of a result, the statement cache and waiting for the pool all have a limit by default, so nothing grows without bound. The one thing MySQL does not bound is a write that holds its locks and runs long; MariaDB bounds that too (see [Limits](#limits)).

## Connecting

```rust
use std::time::Duration;
use giong_mysql::{Config, Error};

fn configs() -> Result<(), Error> {
    let config = Config::from_url("mysql://app:secret@db.internal:3306/shop?ssl-mode=VERIFY_IDENTITY")?;

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

A URL is `mysql://user:password@host:port/database?param=value`. The host can be an IPv6 address in brackets, or empty with the server's Unix socket in a `socket` parameter: `mysql://app@/shop?socket=/var/run/mysqld/mysqld.sock` (in code, a host starting with `/` is a socket). The parameters understood are `ssl-mode`, `ssl-ca`, `socket`, `server-public-key-path`, `connect-timeout` (seconds) and `application-name`. Any other parameter is an error, not silently ignored.

Each connection starts its session with:

- the character set `utf8mb4` (`SET NAMES`), checked in the same round trip: a server that would store text in another character set without a word, because it ignores the handshake's or something undoes `SET NAMES`, is refused (`ErrorKind::Connect`);
- the time zone UTC (`+00:00`), so `TIMESTAMP` columns, `NOW()` and `SystemTime` agree;
- the statement timeout, as `max_execution_time` and `lock_wait_timeout` on MySQL, as `max_statement_time` and `lock_wait_timeout` on MariaDB;
- `autocommit` on, and the session tracker on, which tells the pool when a statement changed the session.

`Config::session_variable("sql_mode", "'STRICT_ALL_TABLES'")` sets other session variables; the ones above are refused there, except `lock_wait_timeout`. A value is one SQL expression: one holding a `;`, a comment, a backslash or a `,` outside quotes and parentheses is refused, so it cannot set a refused variable too. They are all set again whenever a pooled connection's session is reset.

**TLS.** Unless `Config::tls` or `ssl-mode` says otherwise:

| Where the server is | What happens |
|---|---|
| A Unix socket | No TLS. |
| A loopback address (`localhost`, `127.0.0.0/8`, `::1`) | No TLS. |
| Anywhere else | TLS, with the certificate checked against the certificate authorities installed on the machine and against the host name. |

`ssl-ca=/path/ca.pem` trusts only that authority instead of the machine's. `ssl-mode` is one of:

| `ssl-mode` | The certificate is checked against | In code |
|---|---|---|
| `DISABLED` | Nothing: no TLS. | `Tls::Disable` |
| `VERIFY_IDENTITY` | The authorities (`ssl-ca`, or the machine's) and the host name. | `Tls::Verify(TlsConnector::with_root_pem(..)?)` |
| `VERIFY_CA` | The authority in `ssl-ca`, which it needs, and not the host name. | `Tls::verify_ca(..)?` |
| `REQUIRED` | With `ssl-ca`, like `VERIFY_CA`, as in MySQL's own client. Without it, like `VERIFY_IDENTITY` against the machine's authorities, where MySQL's client checks nothing and anyone on the path could pose as the server. | |
| `PREFERRED` | Refused: it falls back to an unencrypted connection without saying so. | |

`VERIFY_CA` is for the certificates MySQL generates when it is installed, which name no host (`MySQL_Server_<version>_Auto_Generated_Server_Certificate`), so a host name check always fails: give it the server's own `ca.pem`, from its data directory. Use it only with an authority that signs for this server alone; with a public one, a certificate it issued for any name would pass. When TLS is requested and the server does not offer it, connecting fails.

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
| `client.insert(..)` | The `AUTO_INCREMENT` id an `INSERT` generated (of its first row), or `None` if it generated none; the statement ran either way. |
| `client.batch_execute(sql)` | Several statements separated by `;`, with no parameters and no results, stopping at the first error. For migrations and setup. |
| `client.transaction()` | Starts a transaction and borrows the client mutably until it ends. `tx.commit()` or `tx.rollback()`; dropping the transaction rolls it back. A transaction the server ended early (deadlock, lock wait timeout with `innodb_rollback_on_timeout`, DDL), or one with a statement given up, refuses further statements, and its `commit` fails. |
| `client.reset_session()` | Resets the session the way the pool does when a connection comes back: rolls back, releases locks, drops temporary tables and variables, and sets the session up again. For a stored function or trigger that takes a named lock or sets a user variable, which the server does not report. |
| `client.cancel_token()` | A `CancelToken` whose `cancel()` runs `KILL QUERY` over a separate connection, stopping the statement running now. It goes to the address the connection reached and only if the server there is the same one, so behind a load balancer it cannot stop another session's statement. A token taken from a pooled connection (`pool.get()`) stops working, sending nothing, once the connection goes back to the pool, and a connection that goes back while such a request is on its way is closed rather than lent to another task. |
| `client.server_version()`, `client.connection_id()` | What the server said when connecting; the id is the one `KILL` and `SHOW PROCESSLIST` use. |
| `client.is_closed()`, `client.in_flight()` | Whether the connection is closed; how many requests are waiting for their answer. |
| `row.get::<T>(column)` | A column by position or name, converted to `T` (see [Types](#types)). Borrowed types (`&str`, `&[u8]`) borrow from the row. |
| `row.is_null(column)`, `row.columns()`, `row.len()` | Whether a value is `NULL`; the columns' names and types; how many there are. |
| `Pool::new(config, max_size)` | A pool for the current worker. Connections are opened when needed, up to `max_size`. Size it to the database server's cores, shared among the workers (see [Performance](#performance)). |
| `.wait_timeout(duration)` | How long `get()` and the statement methods wait when every connection is taken, 5 s by default. |
| `.idle_timeout(duration)`, `.max_lifetime(duration)` | Close a connection unused for this long (10 min by default); replace one older than this (30 min, less up to 10% so that connections opened together are not replaced together). |
| `pool.query(..)` and the other statement methods | Run on the least busy connection, pipelined with other tasks' statements. |
| `pool.get()` | A `PooledClient` for this task alone, which derefs to `Client`. For transactions, session settings and statements known to be slow. Its session is reset when it comes back, if it may have changed. |
| `pool.stats()` | Connections open, taken, being opened, set aside for `get()`; requests in flight; tasks waiting; and counters for connections opened, connect errors, wait timeouts, sessions reset, prepares refused for `max_prepared_stmt_count` and connections recycled. |

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
| `Connect` | [E3102](../docs/errors/E3102.md) | Name resolution, TCP, TLS, or the connect timeout. Also an unknown database (1049), a server older than MySQL 8.0 or MariaDB 10.6, a session whose character set is not `utf8mb4`, and a cancel request that reached another server. |
| `Auth` | [E3103](../docs/errors/E3103.md) | The server refused the credentials, asked for a plugin this crate does not support, or would have needed the password where it could be read. |
| `Db` | [E3104](../docs/errors/E3104.md) | The server reported an error for the statement; `error.server_code()` is MySQL's error number (1062 for a duplicate key, 1213 for a deadlock...) and `error.db()` has the SQLSTATE too. |
| `Type` | [E3105](../docs/errors/E3105.md) | A value could not be converted between Rust and MySQL, or the number of parameters is wrong. The message names the parameter or column and suggests the Rust type to use. |
| `RowCount` | [E3106](../docs/errors/E3106.md) | `query_one` found no row or several, `query_opt` several. |
| `TooLarge` | [E3107](../docs/errors/E3107.md) | The result was larger than `max_result_size`. The connection stays usable. |
| `Closed` | [E3108](../docs/errors/E3108.md) | The connection is closed. A `Pool` replaces it by itself. |
| `PoolTimeout` | [E3109](../docs/errors/E3109.md) | No pooled connection became free within `wait_timeout`. |
| `Protocol` | [E3110](../docs/errors/E3110.md) | The server sent something this crate cannot understand. The connection is closed. |
| `Statement` | [E3111](../docs/errors/E3111.md) | Refused before sending: a transaction statement, or one that changes the session (`SET`, `USE`, `LOCK TABLES`, a temporary table, `GET_LOCK`, a user variable), on the connections a `Pool` shares. Use `pool.get()`, and `transaction()`. |

`error.is_transient()` is true for errors worth retrying after a pause: connecting, a closed connection, a pool wait timeout, and the server's deadlock (1213), lock wait timeout (1205), too many connections (1040, 1203), shutdown (1053), read-only mode (1290 naming `--read-only`, 1792, 1836) and a killed connection (1927, MariaDB). Retry a whole transaction, not its failed statement.

An error from the server affects only its own statement: the statements that other tasks pipelined on the same connection get their own results.

## Limits

| Setting | Default | What happens at the limit |
|---|---|---|
| `Config::connect_timeout` | 10 s | `ErrorKind::Connect`, "connect timeout". |
| `Config::statement_timeout` | 30 s | On MySQL, sent as the session's `max_execution_time`, so the server stops a `SELECT` that runs too long (error 3024), and as `lock_wait_timeout`, so a statement waiting for a table or metadata lock gives up (1205). A write waiting for a row lock gives up after `innodb_lock_wait_timeout` (50 s by default); one that holds its locks and runs long is not stopped. On MariaDB, sent as `max_statement_time`, which stops every statement (1969), and as `lock_wait_timeout`. |
| `Config::max_result_size` | 64 MiB of row data | `ErrorKind::TooLarge`; the rest of the result is read and thrown away. |
| `Config::statement_cache_capacity` | 64 per connection | The least recently used statement is closed. 0 prepares and closes every statement each time. The server's `max_prepared_stmt_count` (16,382 by default) bounds the statements of all its clients: processes × workers × pool size × this must stay below it (64 allows 255 connections). At that limit (error 1461), the connection closes its unused statements and prepares again, once. |
| `Pool::wait_timeout` | 5 s | `ErrorKind::PoolTimeout`. |
| `Pool::idle_timeout` | 10 min | The connection is closed. |
| `Pool::max_lifetime` | 30 min, less up to 10% | The connection is closed once no statement uses it (one taken with `get()`, when it comes back), and a new one is opened when needed. |

Values over 16 MiB are split into packets and joined again, both ways. The server's `max_allowed_packet` (64 MiB by default) limits the size of a statement with its parameters, and of a row.

## Performance

Measured with `bench/mysql.sh` against mysql_async 0.36.2 and SQLx 0.8.6, each with its own pool (Tokio 1.54, current-thread runtime). Each client runs on one core; the same MySQL 8.0.46 server runs on the other three cores of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), reached over TCP on 127.0.0.1. 64 tasks issue statements back to back: `point` reads one row by primary key, `list` reads 100 rows, and every column of every row is decoded (`i64`, two strings, a date and time). The figures are medians of 5 rounds of 5 s (3 rounds for the other pool sizes), with the range in brackets.

A pool, on one client core:

| Client | Connections | `point` statements/s | p99 | `list` statements/s | p99 |
|---|---|---|---|---|---|
| **giong-mysql** | 3 | **150.7k** (142–158k) | **0.81 ms** | **38.9k** (36–40k) | **3.4 ms** |
| giong-mysql | 4 | 152.1k (134–157k) | 4.4 ms | 38.3k (37–41k) | 6.5 ms |
| SQLx | 4 | 17.7k (17–18k) | 4.8 ms | 7.6k (7.0–8.4k) | 13.2 ms |
| SQLx | 64, its best | 29.3k (26–30k) | 3.9 ms | 10.4k (9.0–11.7k) | 9.1 ms |
| mysql_async | 4, its best | 17.1k (16–18k) | 6.4 ms | 9.5k (8.7–10.4k) | 11.2 ms |
| mysql_async, default pool settings | 4 | 11.5k (9.9–12.7k) | 8.7 ms | 6.4k (5.9–7.2k) | 14.2 ms |

- **Against the best of the others (SQLx with 64 connections): 5.1× the point lookups and 3.7× the 100-row reads, at a fifth to a third of the p99.** SQLx's client core is the limit (99–100% busy). giong-mysql sends the statements of all 64 tasks in a few writes and reads their answers in a few reads; here the server is its limit (its three cores 94% busy), not the client core (74–89%).
- **mysql_async** is not limited by its core (66–90% busy) and does best with 4 connections: with 16 it does 16.8k point lookups, with 64 13.7k at a p99 of 28 ms. By default its pool resets every connection it takes back (`COM_RESET_CONNECTION`, which also drops the connection's prepared statements); that costs it a third of its throughput (the last row). The other mysql_async rows turn it off.
- **Size the pool to the database's cores.** With more connections than the server has cores (4 on 3 here), one server thread at a time waits for a core, and the statements pipelined on its connection wait with it: the same throughput, but a p99 of 4.4 ms instead of 0.81. 8 connections give 161.6k at a p99 of 8.2 ms; 2 connections leave a server core idle, 87.9k. With several workers, the connections of all their pools add up. Statements that wait on disk or on locks need more connections than this.
- **One connection shared by every task:** 26.6k point lookups and 10.8k 100-row reads, about what SQLx does with 64 connections, using 29–35% of the client core. The single server thread is the limit. Neither of the others can share a connection between tasks.

Not measured: a server on another machine (network latency, where pipelining should matter more), writes, more than one client core, MySQL versions other than 8.0.46. Results are in `bench/results/mysql/`; pool sizes other than 4 in `pool-size/`.

## Not there yet

Streaming a result one row at a time (a result is collected in full, up to `max_result_size`) and cursors, the result sets after the first from a `CALL`, `LOAD DATA LOCAL INFILE` (the server is never sent a file), compression, several hosts with failover, MariaDB's own extensions (its `client_ed25519` login among them), and authentication plugins other than those above. MySQL 8.0 and later and MariaDB 10.6 and later are supported; older versions are refused when connecting.

## Tests

`cargo test -p giong-mysql`: 74 tests against a real MySQL or MariaDB server that the test binary starts for itself (a throwaway data directory from `mysqld --initialize-insecure` or `mariadb-install-db`, with a user for each authentication plugin and path and one that requires TLS, listening on a Unix socket, 127.0.0.1 and 127.0.0.2), and a second one set up the way some production servers are (`init_connect` setting latin1, `innodb_rollback_on_timeout` on, the certificates MySQL generates itself): every supported type in both directions; conversion errors that name the column; server errors and their fields; pipelining across tasks, and a failing statement in the middle of a pipeline; the statement cache and its bound, counted on the server, and a cached statement after a schema change; values of 16 MiB and more, both ways; the result-size limit; the statement timeout; `KILL QUERY` through a cancel token; a query dropped mid-flight; a connection the server kills; a stored procedure with two results; `LOAD DATA LOCAL INFILE` refused; transactions committed, rolled back, dropped, ended by DDL and by a deadlock; no task or connection left behind after a client is dropped; TLS against the right and the wrong authority and host; `caching_sha2_password` over a socket, TCP and TLS, with RSA (the key asked for on loopback, or configured), and from a non-loopback address (through a forwarder on this machine's network address) where only the fast path may log in; `mysql_native_password`; `sha256_password` only over TLS; fake servers that ask for the password over the network, lack TLS, or predate MySQL 8; the connect timeout; and the pool (spreading statements, exclusive use and the rollback on return, even with `BEGIN` still in flight, waiting, and waking when a dropped statement frees the connection, replacing broken connections, connect errors, a slow statement not holding up the others, transaction statements refused on shared connections and a batch that leaves a transaction open); a connection taken and given back with session variables, a temporary table, user variables, `autocommit` off, `LOCK TABLES` and `GET_LOCK` comes back clean, and one that ran only plain statements keeps its prepared statements; session statements refused on shared connections, and a reset after a shared batch or trigger that changed the session; a failed reset closes the connection; `get()` not starved by 32 tasks of shared statements, waiting tasks served in order, a connection set aside for a task that gave up going back, idle connections closed, and old ones replaced under load and when they come back from `get()`, and lifetimes too long for the clock; text stored as `utf8mb4` where the server would make it latin1; the server-wide limit on prepared statements reached; a cancel request through a round-robin balancer that must not stop another server's statement; lock wait timeouts with and without the rollback of the transaction; a write waiting for a table lock bounded by the statement timeout; a statement given up inside a transaction stopping the commit; and `VERIFY_CA` with the certificates MySQL generates. 44 unit tests: packets and their splitting, the scrambles checked the way the server checks them, RSA-OAEP decrypted by the `openssl` command, URLs, type conversions and how their errors name types, the certificate check of `VERIFY_CA`, the versions refused, which errors are worth a retry and the old name of `server_code`, and random inputs for the packet parsers, the framing, rows of every column type, URLs, key files and the connection's state machine (3,000 sessions against a scripted server that sends mutated answers). 8 documentation examples. The fuzz targets in `fuzz/` cover the same parsers and the state machine (`session`) with coverage guidance.

The tests find `mysqld` through `GIONG_MYSQL_BIN` (the directory that holds it), `/usr/sbin` or `PATH`, and the `mysql` client next to it or on `PATH`. They skip when MySQL is not installed, unless `GIONG_MYSQL_REQUIRED=1` (CI sets it). They also pass under ThreadSanitizer and on the fallback io_uring setups (`scripts/check.sh tsan` and `fallback`). They have passed against MySQL 8.0.46 and MariaDB 10.11 here; CI runs them against MySQL 8.0, 8.4 and 9.1 and MariaDB 10.6, 10.11, 11.4 and 11.8. For MariaDB, point `GIONG_MYSQL_BIN` at the directory that holds `mariadbd`.
