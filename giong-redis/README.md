# giong-redis

A Redis client and connection pool for the [giong](../README.md) runtime. It speaks RESP3 itself, encrypts with [`giong-tls`](../giong-tls/README.md), and depends on nothing from the Tokio ecosystem.

```toml
[dependencies]
giong = "0.5"
giong-redis = "0.5"
```

```rust
use std::time::Duration;
use giong_redis::{cmd, Client, Config};

fn main() -> Result<(), giong_redis::Error> {
    let config = Config::from_url("redis://localhost:6379")?;
    giong::block_on(async {
        let client = Client::connect(&config).await?;
        client.set_ex("session:42", "data", Duration::from_secs(3600)).await?;
        let session: Option<String> = client.get("session:42").await?;
        // Any command, read as the type asked for.
        let top: Vec<(String, f64)> = client
            .query(&cmd("ZRANGE").arg("leaders").arg(0).arg(9).arg("REV").arg("WITHSCORES"))
            .await?;
        println!("{session:?} {top:?}");
        Ok(())
    })
}
```

In a service, make one `Pool` per worker. Nothing is shared between threads, so nothing is locked:

```rust
use giong_redis::{Config, Pool};

fn main() -> std::io::Result<()> {
    let config = Config::from_url("redis://cache.internal:6379")?;
    giong::Builder::new().build()?.run(move |_worker| {
        let pool = Pool::new(config.clone(), 2);
        async move {
            // ... serve requests, each one calling pool.get(..), pool.incr(..) ...
            let hits = pool.incr("hits", 1).await.unwrap();
            println!("{hits} hits");
        }
    })
}
```

## What it does

- **Pipelining across tasks.** Tasks that share a connection send their commands together, and one round trip answers all of them, with no lock and no task in between. One connection serves hundreds of concurrent tasks.
- **Any command, read as the Rust type asked for.** `cmd("ZADD").arg(key).arg(score).arg(member)` builds a command; the reply converts to `Option<String>`, `i64`, `bool`, `Vec<u8>`, `Vec<T>`, `HashMap`, `BTreeSet`, tuples or `Value`. Methods for the everyday commands (`get`, `set`, `set_ex`, `incr`, `hgetall`...).
- **Pipelines and transactions.** `Pipeline` sends several commands at once; `.atomic()` wraps them in `MULTI`/`EXEC`. `watch` gives optimistic transactions with `WATCH`.
- **Commands that would break sharing are refused before they are sent**, with what to use instead. On a shared connection, `SELECT` would move every task to another database, `MULTI` would pull their commands into a transaction, and `BLPOP` would hold them all up for seconds; other clients let that happen.
- **Pub/Sub** on a connection of its own, with patterns. Waiting for a message can be cancelled (a timeout, a `select`) without losing one.
- **A pool per worker.** Commands share the least busy connection and steer away from one stuck behind a slow command; `take()` gives a task a connection to itself, for `WATCH` and blocking commands.
- **Limits everywhere,** all finite by default: connecting, every reply, the size of a reply, waiting for the pool. A reply that times out or is too large fails alone: the connection stays usable for the other tasks.
- **Errors that say what to do,** with the server's error code (`WRONGTYPE`, `NOSCRIPT`...) to match on.
- **ACL users, TLS (always verified), Unix sockets,** and connection URLs.

RESP3 needs Redis 6 or later; Valkey works too.

## Connecting

```rust
use std::time::Duration;
use giong_redis::Config;

let config = Config::from_url("rediss://app:secret@cache.internal:6380/2")?;

let same_place = Config::new()
    .host("cache.internal")
    .port(6380)
    .user("app")
    .password("secret")
    .database(2)
    .response_timeout(Duration::from_millis(500));
```

A URL is `redis://user:password@host:port/database?param=value`, `rediss://` for TLS, and `redis+unix:///run/redis/redis.sock?db=2` (or `unix://`) for a Unix socket; in code, a host starting with `/` is a socket. `redis://:secret@host` gives a password alone, for `requirepass`. The parameters understood are `db`, `client-name`, `connect-timeout` and `response-timeout` (seconds, as `0.5`), and `cacert` (a PEM file of the authorities to trust, for `rediss://`). Any other parameter is an error, not silently ignored.

A connection logs in, names itself and selects its database once, when it opens (`HELLO 3 AUTH ... SETNAME ...`, then `SELECT`), in one round trip.

**TLS** is used for `rediss://` and when `Config::tls` asks for it: the server's certificate is checked against the certificate authorities installed on the machine, or the ones given (`Tls::Verify(TlsConnector::with_root_pem(..)?)`), and against the host name. There is no setting that skips the check. `redis://` and Unix sockets do not use TLS.

## Sharing a connection

Every method of `Client` that sends a command takes `&self`, so tasks share one by cloning an `Rc` or by holding a reference, and their commands are pipelined. A few commands change the connection itself, for every task on it, and are refused (`ErrorKind::Command`, before anything is sent):

| Command | Use instead |
|---|---|
| `MULTI`, `EXEC`, `DISCARD` | `Pipeline::new().add(..).atomic()` |
| `WATCH`, `UNWATCH` | `client.watch(&keys)`, on a connection the task has to itself (`pool.take()`) |
| `SELECT` | `Config::database` |
| `AUTH`, `HELLO` | `Config::user` and `Config::password` |
| `SUBSCRIBE`, `PSUBSCRIBE`, `SSUBSCRIBE` and their opposites | `PubSub::connect` |
| `BLPOP`, `BRPOP`, `BLMOVE`, `BZPOPMIN`, `XREAD ... BLOCK`, `WAIT`... | `client.query_blocking(..)`, which takes `&mut self` |
| `MONITOR`, `SYNC`, `PSYNC`, `REPLCONF`, `QUIT`, `RESET`, `CLIENT REPLY` | Nothing: they make no sense on a shared connection. |

A blocking command inside an atomic pipeline does not block (Redis runs it with a timeout of zero), so it is allowed there.

```rust
use giong_redis::{cmd, Pipeline};

// One round trip, run as one transaction.
let (count, _): (i64, bool) = pool
    .pipeline(
        &Pipeline::new()
            .add(cmd("INCR").arg("visits"))
            .add(cmd("EXPIRE").arg("visits").arg(60))
            .atomic(),
    )
    .await?;

// A job queue: a connection to itself, waiting up to 10 seconds.
let mut conn = pool.take().await?;
let job: Option<(String, String)> = conn
    .query_blocking(&cmd("BRPOP").arg("jobs").arg(10))
    .await?;
```

## API

| | |
|---|---|
| `Client::connect(&config)` | Opens a connection: TCP or Unix socket, TLS, `HELLO`, `SELECT`, all within `connect_timeout`. |
| `client.query::<T>(&cmd(..).arg(..))` | Any command, its reply converted to `T`. |
| `client.get`, `set`, `set_ex`, `set_nx`, `del`, `exists`, `expire`, `incr`, `mget`, `hget`, `hset`, `hgetall`, `publish`, `ping` | The everyday commands, with typed arguments and replies. |
| `client.pipeline::<T>(&pipeline)` | Several commands in one write; `T` reads the array of their replies (a tuple, a `Vec`...). A command that failed fails the conversion with its error; read the replies as `Vec<Value>` to see each one. With `.atomic()`, a transaction. |
| `client.watch(&keys)` | `WATCH`, then `watch.query(..)` to read and `watch.exec(&pipeline)` to run a transaction, which returns `None` when a watched key changed. Dropping the `Watch`, or the `watch` call before it returns, sends `UNWATCH`. |
| `client.query_blocking(..)` | A blocking command, without the response timeout (its own timeout argument bounds it). |
| `client.server_version()`, `server_name()`, `connection_id()` | What the server said when connecting; the id is the one `CLIENT KILL ID` takes. |
| `client.is_closed()`, `client.in_flight()` | Whether the connection is closed; how many commands are waiting for their reply. |
| `PubSub::connect(&config)` | A connection for subscriptions. `subscribe`, `psubscribe`, `unsubscribe`, `punsubscribe`, each returning once the server confirmed it; `next_message()` returns a `Message` with `channel()`, `pattern()` and `payload()`. |
| `Pool::new(config, max_size)` | A pool for the current worker. Connections are opened when needed, up to `max_size`. |
| `.wait_timeout(duration)` | How long `take()` and the command methods wait when every connection is taken, 5 s by default. |
| `pool.query(..)`, `pool.pipeline(..)` and the command methods | Run on the least busy connection, pipelined with other tasks' commands. |
| `pool.take()` | A `PooledClient` for this task alone, which derefs to `Client`. For `WATCH` and blocking commands. A connection whose blocking command was cancelled while still running is closed instead of coming back. |
| `pool.stats()` | Connections open, taken, being opened; commands in flight; tasks waiting; and counters for connections opened, connect errors and wait timeouts. |

A `Client` belongs to the worker that opened it (it is not `Send`, like everything in giong).

## Types

Arguments are anything that implements `ToArg`: strings, byte slices and vectors, integers, floats, and references to them. Replies convert with `FromValue`:

| Rust | Reads |
|---|---|
| `Option<T>` | Nil as `None`, anything else as `T`. A missing key read into a type that is not an `Option` is an error that says so. |
| `String`, `Vec<u8>` | Bulk, simple and verbatim strings; `String` only if they are UTF-8, `Vec<u8>` always. |
| `i8` ... `i128`, `u8` ... `u64` | Integers, and strings that hold one, when the value fits. |
| `f64`, `f32` | Doubles (RESP3), integers, and strings that hold a number (`ZSCORE`'s reply). |
| `bool` | Booleans, and the integers 0 and 1 (`EXISTS`, `SISMEMBER`). |
| `()` | Any reply but an error, discarded (`OK`). |
| `Vec<T>`, `HashSet<T>`, `BTreeSet<T>` | Arrays and sets; a map as `(key, value)` pairs. |
| `HashMap<K, V>`, `BTreeMap<K, V>` | Maps (`HGETALL`, `CONFIG GET`), and flat arrays of keys and values (a stream entry's fields). |
| Tuples of 1 to 6 | An array of exactly that many replies (`(String, f64)` for `WITHSCORES` pairs). |
| `Value` | Any reply, as it arrived. |

A conversion never loses anything silently: a number that does not fit, bytes that are not UTF-8, or an array of the wrong length fails with `ErrorKind::Type` and names what arrived and what to read it as. Implement `FromValue` for your own types.

## Errors

`Error` is cheap to clone, implements `std::error::Error`, and converts into `io::Error`. `error.kind()` says what went wrong. Each kind has a giong error code, shown in front of the message and linked to its page in [docs/errors](../docs/errors/README.md) (`GIONG-E3204: WRONGTYPE Operation against a key holding the wrong kind of value. See …`); after `?` into an `io::Error`, `giong::error::code(&err)` finds it:

| `ErrorKind` | Code | Meaning |
|---|---|---|
| `Config` | [E3201](../docs/errors/E3201.md) | The configuration or URL is not valid. |
| `Connect` | [E3202](../docs/errors/E3202.md) | Name resolution, TCP, TLS, the handshake, or the connect timeout. Also a server older than Redis 6 and a database number the server does not have. |
| `Auth` | [E3203](../docs/errors/E3203.md) | The server refused the user name or the password. |
| `Server` | [E3204](../docs/errors/E3204.md) | The server answered with an error; `error.server_code()` is its code (`ERR`, `WRONGTYPE`, `NOSCRIPT`, `READONLY`...). |
| `Type` | [E3205](../docs/errors/E3205.md) | A reply could not be converted to the Rust type asked for. |
| `Command` | [E3206](../docs/errors/E3206.md) | The command would break sharing the connection, and was not sent; the message says what to use instead. |
| `TooLarge` | [E3207](../docs/errors/E3207.md) | The reply was larger than `max_reply_size`. The connection stays usable. |
| `Closed` | [E3208](../docs/errors/E3208.md) | The connection is closed. A `Pool` replaces it by itself. |
| `PoolTimeout` | [E3209](../docs/errors/E3209.md) | No pooled connection became free within `wait_timeout`. |
| `Protocol` | [E3210](../docs/errors/E3210.md) | The server sent something that is not RESP3. The connection is closed. |
| `Timeout` | [E3211](../docs/errors/E3211.md) | No reply within `response_timeout`. The command may still have run; the connection stays usable. |

`error.is_transient()` is true for errors worth retrying after a pause: connecting, a closed connection, timeouts, and the server's `LOADING`, `BUSY`, `TRYAGAIN`, `MASTERDOWN` and `CLUSTERDOWN`. An error from the server affects only its own command: the commands that other tasks pipelined on the same connection get their own replies.

## Limits

| Setting | Default | What happens at the limit |
|---|---|---|
| `Config::connect_timeout` | 10 s | `ErrorKind::Connect`, "connect timeout". |
| `Config::response_timeout` | 30 s | `ErrorKind::Timeout` for that command; its reply is thrown away when it comes, and the connection goes on. Set it near what the caller can wait, such as 500 ms for a cache. |
| `Config::max_reply_size` | 64 MiB | `ErrorKind::TooLarge`; the reply is read and thrown away as it arrives, without being held in memory. |
| `Pool::wait_timeout` | 5 s | `ErrorKind::PoolTimeout`. |

Replies nested more than 128 deep, and lines over 1 MiB (an error message, a number), are refused as protocol errors.

## Performance

Measured with `bench/redis.sh` against redis-rs 1.8.1 (a `MultiplexedConnection`) and fred 10.1.0 (a `Client`, with its automatic pipelining), both on Tokio 1.54 with a current-thread runtime. All three pipeline the commands of tasks that share a connection. Each client runs on one core; Redis 7.0.15 (no persistence, `io-threads 3`) runs on the other three cores of a 4-vCPU virtual machine (Xeon 2.1 GHz, Linux 6.18), reached over TCP on 127.0.0.1. 64 tasks send commands back to back: `GET` and `SET` of a 64-byte value, `GET` of an 8 KiB value, and `MGET` of ten 64-byte values. Medians of 5 rounds of 5 s, commands per second; `µs/op` is the client core's time per command (user and system).

One connection shared by the 64 tasks:

| Client | `GET` | p99 | µs/op | `SET` | µs/op | `GET` 8 KiB | µs/op | `MGET` ×10 | µs/op |
|---|---|---|---|---|---|---|---|---|---|
| **giong-redis** | **618k** | **184 µs** | **0.78** | **557k** | **0.77** | **304k** | **2.96** | **219k** | **2.29** |
| redis-rs | 448k | 250 µs | 1.76 | 442k | 1.67 | 155k | 5.24 | 140k | 4.84 |
| fred | 314k | 323 µs | 2.42 | 287k | 2.58 | 129k | 5.97 | 110k | 6.91 |

Four connections (giong-redis's `Pool` of 4; four of the others', task `t` on number `t % 4`):

| Client | `GET` | p99 | µs/op | `SET` | µs/op | `GET` 8 KiB | µs/op | `MGET` ×10 | µs/op |
|---|---|---|---|---|---|---|---|---|---|
| **giong-redis** | **881k** | **160 µs** | **0.98** | **779k** | **0.99** | 272k | 3.49 | **313k** | **2.30** |
| redis-rs | 475k | 239 µs | 2.04 | 491k | 1.97 | 269k | 3.61 | 198k | 4.96 |
| fred | 329k | 328 µs | 2.98 | 308k | 3.18 | 169k | 5.74 | 143k | 6.84 |

- **Small commands: 1.3 to 1.9 times redis-rs, at half its CPU time per command or less.** With one connection none of the clients is held back by its core (giong-redis uses about half of it): the round trip is the limit, and giong-redis's is shorter, so its tasks send more. With four connections, redis-rs and fred use all of their core (97–98%), and giong-redis does 1.6 to 1.9 times redis-rs.
- **Large values are mostly the kernel's work.** For 8 KiB values about half of the client's core is time in the kernel, receiving the data, whatever the client. With four connections giong-redis and redis-rs are level (272k and 269k): giong-redis spends less time in its own code (3.1 s against 3.8 s of user time in a 7-second run) and more in the kernel (3.6 s against 3.1 s). With one connection giong-redis does twice what redis-rs does.
- **How many connections.** The server above has I/O threads; most run without them, as Redis does by default. Against Redis without I/O threads (`bench/results/redis/no-io-threads.txt`), giong-redis reads 645k `GET`s per second with one connection, 925k with two, and 845k with four; the server's single thread is then the limit. So a `Pool` of one or two connections per worker is enough for one Redis server, and more only add to its work. redis-rs does best with two too, 542k.

Cutting the client's own cost per command took the profile further than the first version: the command methods write their command straight into the connection's buffer, with no allocation; a reply is not kept in a `Vec` of its own; the read buffer moves what is left of a reply only when it is short of room, instead of on every read; and each connection has one timer, set for the oldest command waiting, instead of one per command (a test checks that every command still times out on its own deadline, and never early). Together they took `GET` with four connections from 660k to 881k per second.

Not measured: a server on another machine (network latency, where pipelining matters more), Redis 7.2 and later, Valkey, TLS, more than one client core, `fred` with its own pool type. Results are in `bench/results/redis/`.

## Not there yet

Redis Cluster (a `MOVED` or `ASK` reply is an error that says so) and Sentinel, client-side caching (`CLIENT TRACKING`), sharded Pub/Sub (`SSUBSCRIBE`), RESP2-only servers (Redis 5 and older), and streaming a large reply in pieces (a reply is collected in full, up to `max_reply_size`). Tested against Redis 7.0 only so far. The crate has had no independent security audit.

## Tests

`cargo test -p giong-redis`: 28 tests against a real Redis server that the test binary starts for itself (a throwaway directory, listening on 127.0.0.1, a TLS port with a certificate from a test authority, and a Unix socket, with an ACL user besides the default one): strings, numbers and missing keys; every RESP3 reply type read into the Rust types; 200 tasks sharing one connection, each getting its own replies; server errors with their codes, leaving the connection usable; pipelines, transactions and a transaction the server refuses as it is queued; `WATCH` when a key changed and when it did not, and an atomic pipeline refusing to report success when a watch left behind stopped it; every command that would break sharing refused before sending; blocking commands on a connection of their own; a reply over the size limit failing alone; client-side caching invalidations over the size limit passed over, not taken for replies; the response timeout, with the late reply thrown away; commands queued behind one that gets no answer each timing out on their own deadline, never early (against a scripted server); a connection the server kills; ACL users and wrong passwords; the database and the client name; Unix sockets and TLS, against the right and the wrong authority, and TLS sent to the plain port and plain to the TLS port; connecting to a closed port, a silent server and a missing socket, fast and with a reason; a server that is not Redis; the pool (sharing a few connections among many tasks, `take()` and the return, a connection left in a cancelled blocking command closed rather than shared, a `watch` cancelled before its reply leaving no `WATCH` behind, a task waiting to take a connection woken when a dropped command frees it, connections the server closed replaced, a pool that cannot connect); and Pub/Sub (channels and patterns, binary payloads, waiting cancelled without losing a message, a subscriber whose connection was killed). 28 unit tests: command encoding and the refusals, URLs, error codes, the reply parser for every RESP3 type, attributes and pushes, replies cut anywhere and the size limit, the read buffer's memory, conversions; and 330,000 random inputs: replies cut at random places and read back with random limits, damaged replies (an error, never a panic) and URLs. 14 documentation examples.

The tests find `redis-server` through `GIONG_REDIS_BIN` (the directory that holds it), `/usr/bin` or `PATH`. They skip when Redis is not installed, unless `GIONG_REDIS_REQUIRED=1` (CI sets it, and installs Ubuntu's Redis 7.0). They also pass under ThreadSanitizer and on the fallback io_uring setups (`scripts/check.sh tsan` and `fallback`). They have passed against Redis 7.0.15; Redis 6, 7.2+ and Valkey are not tested yet.
