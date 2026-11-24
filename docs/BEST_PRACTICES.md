# Best practices

How to build a service on giong that is correct the first time, fast without tuning, and
easy to run. Each rule says why, so that a person or an agent can tell when it applies.
The crates' READMEs have the details; the [agent skill](https://giong.run/docs/agent-skill/)
carries these rules for coding agents.

## The shape of a service

Start one runtime, and build everything a request needs inside each worker's future:

```rust,ignore
use std::time::Duration;
use giong::net::TcpListener;
use giong_postgres::{Config, Pool};
use giong_web::middleware::{Logger, RequestId, Timeout};
use giong_web::{Router, Server, Sink};

fn main() -> std::io::Result<()> {
    let db = Config::from_url(&std::env::var("DATABASE_URL").expect("DATABASE_URL"))?;
    let runtime = giong::Builder::new()
        .shutdown_on_signals(true)                 // SIGINT and SIGTERM (Ctrl+C on Windows)
        .shutdown_grace(Duration::from_secs(10))   // answer the requests in flight first
        .build()?;
    let _mcp = giong_mcp::start_from_env(&runtime.handle()); // off unless GIONG_MCP is set
    runtime.run(move |_worker| {
        let pool = Pool::new(db.clone(), 4);       // one pool per worker
        async move {
            let app = Router::new()
                // .get("/users/{id}", get_user) ...
                .layer(RequestId::new())
                .layer(Logger::new(Sink::Stdout))
                .layer(Timeout::new(Duration::from_secs(5)))
                .build(pool);                      // the state handlers read with req.state()
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, app).await;
        }
    })
}
```

- **One worker per core, and nothing shared between them.** Each worker binds its own
  listener on the same port, keeps its own pools and its own router, and serves the
  connections it accepted until they close. That is why futures need not be `Send` and why
  nothing is locked on the hot path. Build per-worker things inside the closure; clone
  configuration into it.
- **`bind` takes IP addresses.** `"0.0.0.0:8080"` or `"[::]:8080"`, never `"localhost:8080"`
  (refused with `GIONG-E0100`). `TcpStream::connect` resolves host names itself.
- **Let giong pick the number of workers.** It starts one per CPU the process may use,
  lowered by a container's CPU quota. Set `Builder::workers(n)` only when you know better,
  for example a Kubernetes CPU request with no limit.
- **Stop gracefully.** `shutdown_on_signals(true)` and `shutdown_grace(..)`; in your own
  accept loops, wrap the wait in `giong::until_shutdown(..)`. Without them, a program that
  runs as process 1 in a container ignores `SIGTERM` until it is killed.

## State

- **Per worker: `Rc<RefCell<T>>`**, built inside the worker's future. Never hold a
  `borrow_mut()` across an `.await`: another task on the same worker would panic on it.
- **Across workers: immutable data in an `Arc`, atomics, or channels.** `giong::sync` has
  `mpsc` (unbounded and bounded) and `oneshot`. Tokio's `sync` module (`Semaphore`,
  `Notify`, `watch`, `broadcast`) needs no Tokio runtime and works on giong:
  `tokio = { version = "1", default-features = false, features = ["sync"] }`.
- **Work for another worker** goes through `Handle::spawn_on(worker, || async { .. })`, or
  `Handle::spawn_balanced(|| async { .. })` for the first idle one. A task never moves once
  it has started.

## Never block a worker

A worker runs every task of the connections it owns. A blocking call or a long computation
stops all of them, and their latency becomes yours.

- Use `giong::fs` for files, `giong::time::sleep` for waiting and `giong::net` for
  sockets, never `std::fs`, `std::thread::sleep` or `std::net` on a worker.
- Run blocking calls (`std::fs`, C libraries) with `giong::spawn_blocking` and CPU-heavy
  work (hashing, images, compression) with `giong::spawn_compute`, whose threads run below
  the workers' priority, and await the answer. `try_spawn_compute` sheds with
  `GIONG-E0014` when the compute lane is behind, for a service that would rather answer
  `503` than queue.

```rust,ignore
// Err(JoinError) only if the closure panicked or the runtime stopped first.
let hash = giong::spawn_compute(move || expensive(input)).await;
```

- A loop that does a lot of work without waiting on I/O should call
  `giong::yield_now().await` now and then.
- Thread-per-core has a cost to know about: a short request waits behind a long one on the
  same core, where Tokio's work stealing would move it. With one request in twenty
  computing for a millisecond, the short requests' p99 on giong was 1.3 to 1.7 times the
  best runtime's ([benchmark lab](BENCHMARKS.md#benchmark-lab)). Move heavy computation off
  the workers.
- To find a blocked worker, ask the service: giong-mcp's `overview` warns about it and
  names the line that spawned the task ([Observe](#observe-the-running-service)).

## Timeouts, limits and cancellation

- **Put a deadline on every wait for another party**: `giong::time::timeout(duration, fut)`.
  giong-web's `Timeout` middleware does it per request; the database clients have connect
  and statement timeouts in their `Config`.
- **Know what is cancel safe.** `recv`, `recv_from` and `accept` are: a timeout around them
  loses no data, datagram or connection. One-off reads (`read`, `read_from`, `read_at` on a
  pipe) are not, and a dropped `write_all` may have sent part of its buffer.
- **Keep giong-http's limits.** Every timeout and size limit is finite by default. Tune them
  for the service rather than turning them off: `Server::request_timeout`, `idle_timeout`,
  `max_head_size`, `max_body_size`, `max_connections` (10,000 per `serve` call) and
  `min_free_memory`, which refuses new work before the process runs out of memory.

## Buffers

- **`recv()` lends a buffer from the worker's pool.** Copy what you need and drop it; do not
  hold it across an `.await`. An empty buffer means the peer closed the connection.
- **Writes take their buffer by value and hand it back**:
  `let (result, buf) = stream.write_all(buf).await;`. Reuse `buf` for the next write.
- **Bytes that cannot change go out without a copy.** TCP sends of 16 KiB or more from
  `&'static [u8]`, `Rc<[u8]>` or `Arc<[u8]>` use zero-copy send on Linux 6.2 and newer; a
  `Vec` is copied.
- **Datagrams**: one received with `recv_from()` lives in a pool buffer, so its longest
  payload is the buffer size minus 44 bytes (16 KiB − 44 by default) and `is_truncated()`
  says when it was cut. Raise `Builder::recv_buffers(count, size)` or use `read_from()` with
  a buffer of your own; do not mix the two on one socket.
- **A `File` has no cursor**: every read and write names its offset.

## Errors

- **Match on the code, never on the text.** Every error giong raises starts with a code
  (`GIONG-E0102: no address found for db.internal. Fix: ...`) and links to its page in
  [docs/errors](errors/README.md). `giong::error::code(&err)` returns it as a `Code`.
- **In giong-web handlers, return `Result<Response, Error>`** and use `?`. A giong error
  anywhere down the `source()` chain becomes the right status (a database that cannot be
  reached is a `503` with `Retry-After`), any other error a `500` whose cause is logged and
  never sent. Answers are problem+json with a `GIONG-E` code.
- **When an error is new to you, read its page**: it says what went wrong and how to fix
  it, often with the one line to change.

## Databases and Redis

- **One `Pool` per worker, and small.** The statements of every task on a worker share the
  pool's few connections and are pipelined, which is where giong-postgres's and
  giong-mysql's factors over deadpool and SQLx come from. Size a pool for the transactions
  that run at once, plus one; three or four connections per worker is a good start.
- **Standalone statements on the pool, everything else on a taken connection.**
  `pool.query(..)` and friends refuse statements that change the session (`BEGIN`, `SET`,
  `LISTEN`, temporary tables, `USE`). Run transactions and session changes on
  `pool.get().await?`; the connection comes back with its session reset.
- **Batched reads** (`Config::batch_reads(true)` in giong-postgres) pay off when the
  database is the busy side: 3.4 times the one-row lookups on one server core. Leave them
  off where row-level security or other settings are made per request inside a function.
- **TLS is verified by default** for remote hosts. Keep it that way.
- **Redis**: the commands of every task pipeline over the pool's connections; use `take()`
  for a connection of your own (Pub/Sub, `WATCH` transactions).

## HTTP clients

- **One `Client` per worker, reused.** It pools connections per origin and multiplexes
  HTTP/2. A new client per request pays the TCP and TLS handshakes every time.
- `Client::preconnect(url)` at start-up opens the connection to an origin you will need
  before the first request does; `ping_idle_http2(url, deadline)` finds a dead idle HTTP/2
  connection before a request does.
- Use giong-http's client, not reqwest: crates that need the Tokio runtime panic on a giong
  worker with "there is no reactor running".

## Choose the giong crate first

For each job, take the first that fits ([Choosing crates](../README.md#choosing-crates)):

1. **A giong crate**: giong-http (server and client), giong-web, giong-tls, giong-grpc,
   giong-proto, giong-quic, giong-postgres, giong-mysql, giong-redis, `giong::fs`,
   `giong::net::lookup_host`, `giong::time`.
2. **A crate that needs no runtime**: serde, rustls, `futures`' combinators
   (`futures::join!`, `futures::select!`), Tokio's `sync` module.
3. **A Tokio-ecosystem crate that takes a stream from you**, through
   `giong_compat::Compat::new(stream)`, only for a protocol giong has no crate for
   (WebSocket with tokio-tungstenite, for one).
4. **Never a crate that needs the Tokio runtime** on a worker (sqlx, reqwest, tonic's
   transport, most cloud SDKs). If you must use one, run it on a Tokio runtime in a thread
   of its own and pass work to it over a channel.

## Observe the running service

- **Log the driver at start-up**: `eprintln!("giong driver: {}", giong::driver_choice());`
  says which driver each worker got and why io_uring is not used.
- **JSON-line access logs** with giong-web's `Logger::new(Sink::Stdout)` and `RequestId`;
  `tracing` spans with its `tracing` feature.
- **Keep giong-mcp in every service.** `let _mcp = giong_mcp::start_from_env(&handle);` costs
  nothing until `GIONG_MCP` is set. Then a coding agent can read each worker's state, the
  live tasks by the line that spawned them (a count that keeps growing is a leak), the pools
  and a profile of where the time goes (commands below). It listens on loopback or a Unix
  socket only; reach a remote service through `ssh -L` or `kubectl port-forward`.

```sh
GIONG_MCP=127.0.0.1:7777 GIONG_MCP_TOKEN=$(openssl rand -hex 16) cargo run --release
claude mcp add --transport http giong http://127.0.0.1:7777/mcp \
    --header "Authorization: Bearer $GIONG_MCP_TOKEN"
```

## Deploy

- **Production runs on Linux.** macOS is for developing and testing on a Mac; on Windows
  only the runtime crate runs ([Platforms](../README.md#platforms)).
- **Containers need no settings**: where io_uring is blocked (Docker's and Kubernetes'
  default seccomp profiles, gVisor), giong runs on epoll and says so once on stderr. Allow
  io_uring for its full speed with the profile in [DEPLOYMENT.md](DEPLOYMENT.md), and
  raise the locked-memory limit on the kernels that count the rings against it.
- **Insist on io_uring where you measured with it**: `GIONG_DRIVER=io_uring` makes the
  runtime refuse to start without it (`GIONG-E0001`) instead of falling back quietly.
- **One process owns a port.** `bind` refuses a port another process holds, so a service
  started twice fails the second time instead of quietly splitting the traffic. Stop the old
  process before starting the new one, or run the two on different ports behind the load
  balancer; [DEPLOYMENT.md](DEPLOYMENT.md#restarting-on-the-same-port) has the other ways and
  what the load balancer should do.

## Performance

- **Measure before you tune.** The defaults are the tuned configuration: `DEFER_TASKRUN`
  rings, cores pinned unless a CPU quota is smaller than the CPU set, buffers that grow with
  the traffic.
- **`Builder::spin_before_park(duration)`** lowers latency at low load (19 µs median
  instead of 28 on one connection) by keeping the worker awake, which costs CPU. Use it on
  dedicated cores for latency-sensitive services, not on shared machines.
- **`Builder::precise_timers(true)`** on macOS makes sleeps and timeouts up to 100 ms end on
  time instead of up to 2 ms late (macOS adds timer slack to batch wake-ups). It costs CPU
  and battery (about a third more CPU in a tight 2 ms sleep loop), so it is off by default;
  Linux and Windows ignore it.
- **`TcpListener::bind_balanced`** for long-lived connections of uneven weight (gRPC,
  streaming): busy workers hand new connections to idle ones. The kernel's spread is fine for
  short HTTP requests.
- **Files to the network**: `Body::file` in giong-http sends with `sendfile`; kernel TLS
  (`Ktls::Auto` in giong-tls) pays off for large files only, so it is off by default.
- **Instruction counts, not wall time, for small changes**: a percent or two is below the
  noise of a benchmark run; `bench/instr` counts instructions instead.

## Test

- **`giong::block_on(fut)`** runs a test's future on one worker; there is no test macro.
- **giong-web handlers without a socket**: build the `App` and call
  `app.handle(Request::from_bytes(..))`; check the status and `Error::of(&resp)` codes.
- **Leaks**: `giong::observe::worker_stats()` gives the current worker's counters (tasks,
  operations in flight), to compare before and after.
- **Both Linux drivers**: run the suite once more with `GIONG_DRIVER=epoll`, which is what
  a container without io_uring will use.

## Upgrade

- **Every 1.x release only adds.** `cargo update` within 1.x needs no code change; the
  CHANGELOG lists what each release added.
- **Unstable features may change**: `unstable-http3` and `unstable-udp-offload` are outside
  the stability promise until they are stabilised.
- **Rust 1.80** for every crate, **1.85** for giong-quic and `unstable-http3`.

## Before you call it done

A short list for people and agents reviewing a change to a giong service:

1. Nothing blocks a worker: no `std::fs`, `std::net`, `thread::sleep` or long loop without
   a yield on it.
2. Every wait for another party has a deadline.
3. No `RefCell` borrow is held across an `.await`.
4. Pools, clients and routers are built per worker.
5. Errors are matched by `giong::error::code`, not by text.
6. Transactions and session changes run on `pool.get()`, never on the pool itself.
7. No crate that needs the Tokio runtime runs on a worker.
8. The tests pass on both drivers (`GIONG_DRIVER=epoll`).
