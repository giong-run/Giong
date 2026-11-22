<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-dark.svg">
    <img alt="giong" src="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-light.svg" width="340">
  </picture>
</p>

<p align="center"><b>An async runtime for Rust services, built for code that AI agents write: a strict API meant to stay correct for years, errors that say how to fix them, and one thread per core on io_uring underneath. Linux first; macOS and Windows too.</b></p>

<p align="center">
  <a href="https://github.com/giong-run/Giong/actions/workflows/main.yml"><img alt="CI" src="https://github.com/giong-run/Giong/actions/workflows/main.yml/badge.svg"></a>
  <a href="https://crates.io/crates/giong"><img alt="crates.io" src="https://img.shields.io/crates/v/giong"></a>
  <a href="https://docs.rs/giong"><img alt="docs.rs" src="https://img.shields.io/docsrs/giong"></a>
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
  <img alt="Platforms: Linux, macOS, Windows" src="https://img.shields.io/badge/platforms-Linux%20%7C%20macOS%20%7C%20Windows-informational">
</p>

Named after Thánh Gióng, the child of Vietnamese legend who lay silent for three years and
then rose a giant: giong costs next to nothing while idle and uses every core the moment
load arrives.

> **Status: 1.4.0.** The API has been frozen since 1.0 ([STABILITY.md](docs/STABILITY.md)),
> and every 1.x release only adds. A whole service runs on giong with no Tokio underneath:
> TCP, UDP, Unix sockets, files, timers, DNS, multi-core and graceful shutdown in the
> runtime, and twelve companion crates for HTTP/1.1, HTTP/2 and HTTP/3, TLS, gRPC,
> Protocol Buffers, QUIC, PostgreSQL, MySQL, Redis, routing and middleware, and an MCP
> server for agents. One service uses it so far, its author's. See
> [Platforms](#platforms) and [Limitations](#limitations).

**New in 1.x**, each a minor release that needs no code change to upgrade:

| Release | What it added |
|---|---|
| **1.4** | Windows: a completion-port (IOCP) driver for the runtime crate: tasks, timers, channels, TCP, UDP, files, DNS and Ctrl+C. The other crates come to Windows in later 1.x releases. |
| **1.3** | HTTP/3: [`giong-quic`](giong-quic/README.md) (QUIC on giong's UDP sockets) and giong-http's HTTP/3 server and client behind the `unstable-http3` feature; UDP offloads (GSO, GRO, ECN) behind `unstable-udp-offload`. |
| **1.2** | macOS: a kqueue driver, tested on macOS 15 and 26 on Apple Silicon; a send or connect that meets `ENOBUFS` waits for buffers ([GIONG-E0028](docs/errors/E0028.md)). |
| **1.1** | gRPC: [`giong-grpc`](giong-grpc/README.md), [`giong-proto`](giong-proto/README.md) and [`giong-grpc-build`](giong-grpc-build/README.md); HTTP trailers and streamed request bodies; `Client::preconnect` and HTTP/2 pings; batched PostgreSQL reads; uploads of 1 MiB 2.2 times as fast. |

The full list is in [CHANGELOG.md](CHANGELOG.md).

## Start here

For people and for AI agents alike, the one path:

1. **Add the crates for the job.** A web service: `cargo add giong giong-web serde --features serde/derive`.
   A bare HTTP server: `cargo add giong giong-http`. The table under
   [Choosing crates](#choosing-crates) says which giong crate does each job and what it
   replaces.
2. **Start one worker per core** with `giong::Builder::new().shutdown_on_signals(true).build()?.run(|_worker| async { .. })`.
   Each worker binds its own listener on the same port; there is no `#[main]` macro.
3. **Read the error.** Every error giong raises starts with a code
   (`GIONG-E0102: no address found for db.internal. Fix: ...`) and links to its page in
   [docs/errors](docs/errors/README.md). Match on `giong::error::code(&err)`, never on the text.
4. **Ask the running service** when something is slow or stuck: [`giong-mcp`](giong-mcp/README.md)
   lets your coding agent read each worker, task and pool ([below](#let-your-agent-look-inside-the-running-service)).
5. **Deploy anywhere Linux runs.** In Docker it works with no settings; allowing io_uring
   makes it faster ([Run in Docker](#run-in-docker)).

[AGENTS.md](AGENTS.md) and [giong.run/llms.txt](https://giong.run/llms.txt) tell a model
the facts it gets wrong; the API reference is on [docs.rs](https://docs.rs/giong).

## What giong is for

giong is a late entrant: Tokio has been Rust's default runtime for years, and giong does not
try to be a smaller copy of it. It is built for the way services are written now, with more
and more of the code coming from AI agents. So the API is designed to be used correctly on
the first try, and to tell the one who got it wrong how to fix it, rather than to be short
to type.

- **The goal: the best async runtime for Rust, on technical merit.** A complete,
  general-purpose runtime for any workload a service has, faster and leaner than Tokio. Every
  claim comes from a benchmark in this repository, and where giong is not ahead the README
  says so (see below).
- **A whole service with no Tokio underneath.** HTTP/1.1 and HTTP/2 (server and client),
  TLS, PostgreSQL, MySQL, Redis, DNS, routing and middleware are giong crates, written for
  its buffer pool and its batched system calls, not adapters. [Choosing crates](#choosing-crates)
  measures each one against the Tokio crate it replaces.
- **Agent-first.** Every error giong raises carries a stable code, the cause and the fix,
  and has a page of its own ([docs/errors](docs/errors/README.md)). [AGENTS.md](AGENTS.md)
  and [giong.run/llms.txt](https://giong.run/llms.txt) tell a model how to work with it;
  giong-web writes JSON-line logs and `tracing` spans; [`giong-mcp`](giong-mcp/README.md)
  lets an agent look inside a running service ([below](#let-your-agent-look-inside-the-running-service)).
- **One major version for a long time.** After 1.0, giong grows through 1.x minor releases
  for years rather than moving to a 2.0, so the API follows four rules:
  1. One way to do each thing.
  2. Misuse fails at compile time, or at run time with a coded error.
  3. Every behaviour code relies on is documented.
  4. Releases only add; they never change what existing code means.

  The API was settled in 0.7 ([review](docs/reviews/2026-11-api-freeze.md)), and
  `cargo-semver-checks` runs on every pull request.
- **Linux first, and the same program elsewhere.** Tuned and measured on Linux with
  io_uring, with an epoll driver wherever io_uring is blocked. macOS runs on a kqueue
  driver, for developing on a Mac; Windows runs the runtime crate on I/O completion ports.
  One API on all of them: where a system cannot do what Linux does, the difference is a
  documented cost, never a different result ([Platforms](#platforms)).

## Let your agent look inside the running service

[`giong-mcp`](giong-mcp/README.md) is an MCP server built into your service. Connect your
coding agent (Claude Code, or any client that speaks MCP's Streamable HTTP) and it can see
what each worker is doing, which tasks are alive and which line of your code spawned them,
what the database pools hold and where the time goes, instead of guessing from logs. The
answer no other tool gives: **which task blocks a worker**, named by the line that spawned
it.

1. Add `giong-mcp` and start it next to the runtime. It starts only when `GIONG_MCP` is
   set, and never stops the service from starting:

   ```rust,no_run
   fn main() -> std::io::Result<()> {
       let rt = giong::Builder::new().shutdown_on_signals(true).build()?;
       let _mcp = giong_mcp::start_from_env(&rt.handle()); // keep it: dropping it stops the server
       rt.run(|_worker| async {
           // ... the service ...
           giong::shutdown_signal().await;
       })
   }
   ```

2. Run the service with an address and a token (16 characters or more):

   ```sh
   export GIONG_MCP=127.0.0.1:7777 GIONG_MCP_TOKEN=$(openssl rand -hex 16)
   cargo run --release
   ```

3. Connect the agent, with the same token:

   ```sh
   claude mcp add --transport http giong http://127.0.0.1:7777/mcp \
       --header "Authorization: Bearer $GIONG_MCP_TOKEN"
   ```

The agent then has five tools: `overview` (memory, threads, open descriptors against the
limit, each worker's state, warnings such as a blocked worker), `workers` (each worker's
counters, timers, I/O in flight and the task it is stuck in), `tasks` (live tasks grouped by
the line that spawned them: a count that keeps growing is a leak), `sources` (the
PostgreSQL, MySQL and Redis pools: open, taken, waiting, timeouts) and `profile` (times every
poll for a window and lists where the workers spent it). Every tool only reads, apart from
`profile` turning on poll timing for its window. The server listens on a loopback address
or a Unix socket only, needs the token on TCP, and runs on a thread of its own, so it answers
even when every worker is stuck. To reach a service on another machine, forward the port
(`ssh -L`, `kubectl port-forward`).

## Why giong

- **One system call per tick, not per operation.** Every read, write, accept, close and
  cancellation a turn of the event loop produces goes to the kernel in a single
  `io_uring_enter`, which also returns every completion that is ready. A request costs
  about **0.03 system calls**; an epoll-based server needs 2.
- **A latency dial.** `Builder::spin_before_park` lets a worker watch its ring instead of
  going to sleep. On a single connection that is **48% more requests and a third less
  latency** (19 µs instead of 28 µs) than without it, and than any other runtime measured
  here.
- **It holds up when connections pile up.** At 5,000 connections giong spends the least
  CPU per request (11% below the next runtime) and has the lowest p99. At 1,000 it is level
  with a hand-written epoll server and ahead of the rest.
- **HTTP and HTTPS in the box.** [`giong-http`](giong-http/README.md) and
  [`giong-tls`](giong-tls/README.md) serve HTTPS at **146.6k requests/s on one core,
  against 81.2k for Axum** on the same rustls.
- **Cancellation-safe by construction.** Dropping an I/O future never frees memory the
  kernel may still write to, and wrapping `recv`, `recv_from` or `accept` in a timeout
  loses neither data nor connections.
- **Runs where io_uring cannot.** In a Docker container with the default seccomp profile,
  under gVisor or on a kernel older than 6.0, a worker switches to an epoll driver by
  itself: same API, same tests, and as fast as raw Tokio there. See
  [Drivers](#drivers).
- **gRPC without copies.** [`giong-grpc`](giong-grpc/README.md) reads messages as
  [`giong-proto`](giong-proto/README.md) views over the receive buffer: a one-thread server
  does 30% to 50% more 100-byte unary calls than tonic and holds 5.7 kB per open stream
  against tonic's 11.6 kB.
- **Small enough to read.** Three dependencies on any one system (`socket2`, plus `libc`
  and `io-uring` on Linux, `libc` on macOS, `windows-sys` on Windows), and a comment on
  every `unsafe` block.

What it is **not**, today: faster than everything at everything. On the classic
50-connection benchmark giong, a tuned Monoio and raw Tokio are within noise of each other,
because the kernel's TCP stack, not the runtime, is most of the cost. Over one PostgreSQL
connection shared by every task, giong-postgres is 7–9% behind tokio-postgres; a blocking
`std` read of a cached file beats every async one, giong's included; and a TLS handshake
costs the same as Axum's once both prefer the same cipher suite. The
[benchmark lab](docs/BENCHMARKS.md#benchmark-lab), which drives giong, Tokio, Monoio and
Glommio with the same program and a neutral load generator, also finds giong behind: Monoio
serves 13% more requests on one core; Tokio receives 1 MiB uploads 1.7 times as fast on the
bare runtime; and with one request in twenty computing for a millisecond, the short
requests' p99 on giong is 1.3 to 1.7 times the best runtime's (Tokio's work stealing, on two
cores). macOS and Windows are correct but not tuned,
and have no published speed numbers yet. The numbers below say where the design pays off
and where it does not.

## Results at a glance

One core per server, loopback, medians of five interleaved rounds. Full tables and method
under [Benchmarks](#benchmarks).

| | giong | Monoio (tuned) | Raw Tokio | Hand-written epoll | Glommio |
|---|---|---|---|---|---|
| System calls per request | 0.03 | 0.04 | 2.05 | 2.03 | 2.10 |
| 1 connection: requests/s | **47.9k** ¹ | 32.3k | 31.0k | 31.9k | 27.4k |
| 1 connection: median latency | **19 µs** ¹ | 28 µs | 29 µs | 29 µs | 33 µs |
| 50 connections: requests/s | 171.7k (204.8k ¹) | 176.1k | 169.6k | 157.2k | 137.4k |
| 50 connections, 8 KB responses: requests/s | 63.5k | 64.4k | 35.7k | 37.1k | 36.4k |
| 1,000 connections: CPU per request | 4.39 µs | 4.48 µs | 4.74 µs | 4.40 µs | 5.66 µs |
| 1,000 connections: p99 latency | **5.9 ms** | 8.1 ms | 50.0 ms | 7.3 ms | 10.1 ms |
| 5,000 connections: CPU per request | **4.42 µs** | 5.25 µs | 5.74 µs | 4.99 µs | 8.29 µs |
| 5,000 connections: p99 latency | **26 ms** | 44 ms | 54 ms | 32 ms | 71 ms |

¹ with `spin_before_park(200 µs)`. Without it giong does 32.4k requests/s at 28 µs on one
connection, the same as the others.

Against the Tokio crate each giong crate replaces, one core each (details and losses in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md)):

| Job | giong | Against | Result |
|---|---|---|---|
| HTTPS, kept-alive `/json` | giong-http + giong-tls | Axum + rustls | 146.6k against 81.2k requests/s (1.8×) |
| gRPC, 100-byte unary calls | giong-grpc | tonic 0.14 | 1.3 to 1.5× the calls per second |
| PostgreSQL, one row by key | giong-postgres | tokio-postgres + deadpool | 311.4k against 93.8k statements/s (3.3×); 7–9% behind on one shared connection |
| MySQL, one row by key | giong-mysql | SQLx | 150.7k against 29.3k statements/s (5.1×) |
| Redis `GET` | giong-redis | redis-rs | 640k against 492k commands/s at half the CPU per command |
| Files, 4 KiB at an offset | `giong::fs` | `tokio::fs` | 1.33M against 163k reads/s (8×); level with blocking `std` |
| Idle connection memory | giong-http | Axum | 2.0 KiB against 20.4 KiB |

## Platforms

| | Linux | macOS | Windows |
|---|---|---|---|
| Driver | io_uring on 6.0 or newer (6.1+ recommended); epoll elsewhere, tested down to 5.15 | kqueue | I/O completion ports (IOCP) |
| Crates | all thirteen | all thirteen | `giong` (the runtime) only, from 1.4 |
| Tested in CI on | 6.18, real 5.15, 6.0, 6.1, 6.12 and Ubuntu 24.04 kernels, Docker's default seccomp profile | macOS 15 and 26, Apple Silicon | Windows Server 2022 and 2025 |
| Tuned and measured | yes: every published number | correct, not tuned, no speed claim | correct, not tuned, no speed claim yet |
| Not there | | kernel TLS; CPU pinning; GSO/GRO | Unix sockets; the HTTP, TLS, database, gRPC, QUIC and MCP crates; processor groups beyond 64 CPUs |
| Use it for | production | developing and testing on a Mac | services that have to run on Windows, on the runtime crate |

- **Rust 1.80** or newer for every crate except [`giong-quic`](giong-quic/README.md), and
  with it giong-http's `unstable-http3` feature, which need **1.85**. How this minimum, the
  kernel minimums and the API move during 1.x: [docs/STABILITY.md](docs/STABILITY.md).
- **io_uring is often blocked** by default (Docker's and containerd's seccomp profiles,
  gVisor, the `kernel.io_uring_disabled` sysctl). giong then runs on epoll without being
  told and says so once on stderr; [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) shows how to
  allow io_uring in Docker, Docker Compose and Kubernetes.
- The differences from Linux on macOS and Windows, area by area, are in
  [docs/design/portability.md](docs/design/portability.md).
- On a Windows machine, a service that needs the other crates runs under WSL2 (`uname -r`;
  `wsl --update` for a 6.x kernel), in a Linux virtual machine or in Docker.

## Quick start

A JSON API on every core, with [`giong-web`](giong-web/README.md):

```bash
cargo new hello && cd hello
cargo add giong giong-web serde --features serde/derive
```

```rust,no_run
use giong::net::TcpListener;
use giong_web::middleware::{Logger, RequestId};
use giong_web::{json, Error, Req, Response, Router, Server, Sink};
use serde::Serialize;

#[derive(Serialize)]
struct User { id: u64, name: String }

async fn get_user(req: Req<()>) -> Result<Response, Error> {
    let id: u64 = req.param("id")?;   // "abc" is a 400 that names `id`
    if id == 0 {
        return Err(Error::not_found("user_not_found", format!("no user {id}")));
    }
    json(200, &User { id, name: "Ada".into() })
}

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)                  // Ctrl-C / SIGTERM stop it gracefully
        .build()?
        .run(|_worker| async {
            let app = Router::new()
                .get("/users/{id}", get_user)
                .layer(RequestId::new())
                .layer(Logger::new(Sink::Stdout))   // one JSON line per request
                .build(());
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, app).await;
        })
}
```

```bash
cargo run --release
curl localhost:8080/users/7    # {"id":7,"name":"Ada"}
```

From a clone of this repository, the whole test suite and the examples:

```bash
git clone https://github.com/giong-run/Giong.git && cd Giong
cargo test --workspace --all-features   # the whole suite, every crate
cargo run --release --example echo      # TCP echo server on port 7000
cargo run --release --example http      # hand-written HTTP/1.1 server on port 3006
cargo run --release --example udp_echo  # UDP echo server on port 7001
cargo run --release --example cat -- Cargo.toml   # read a file through the ring
```

A whole service, with PostgreSQL, a Redis cache and HTTPS, is in
[examples/rest-api](examples/rest-api/README.md).

An HTTP service on every core, with [`giong-http`](giong-http/README.md):

```rust,no_run
use std::time::Duration;
use giong::net::TcpListener;
use giong_http::{Request, Response, Server};

async fn handler(req: Request) -> Response {
    match (req.method(), req.path()) {
        ("GET", "/health") => Response::text("ok"),
        ("POST", "/echo") => Response::new(200).body(req.body().to_vec()),
        _ => Response::new(404),
    }
}

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)                  // Ctrl-C / SIGTERM stop it gracefully
        .shutdown_grace(Duration::from_secs(10))    // requests in progress are answered
        .build()?
        .run(|_worker| async {
            // Every worker binds its own listener on the same port (another process
            // cannot take it); the kernel spreads connections across cores.
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, handler).await;
        })
}
```

For HTTPS, turn on the `tls` feature and call `serve_tls` with a
[`giong_tls::TlsAcceptor`](giong-tls/README.md).

The runtime itself, without the library: a TCP echo server.

```rust,no_run
use giong::net::TcpListener;

fn main() -> std::io::Result<()> {
    giong::Builder::new()
        .shutdown_on_signals(true)
        .build()?
        .run(|_worker| async {
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            while let Some(Ok(stream)) = giong::until_shutdown(listener.accept()).await {
                giong::spawn(async move {
                    loop {
                        let Ok(data) = stream.recv().await else { return };
                        if data.is_empty() { return }       // peer closed
                        let reply = data.to_vec();
                        drop(data);                          // buffer back to the pool
                        if stream.write_all(reply).await.0.is_err() { return }
                    }
                });
            }
        })
}
```

UDP and files: receiving borrows a buffer from the worker's pool, and whatever you hand
to the kernel goes in by value and comes back with the result.

```rust
use giong::{fs, fs::File, net::UdpSocket};

async fn demo() -> std::io::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:9000")?;
    let (datagram, peer) = socket.recv_from().await?;      // multishot, cancel safe
    let reply = datagram.to_vec();
    drop(datagram);                                        // buffer back to the pool
    socket.send_to(reply, peer).await.0?;

    let file = File::create("log.txt").await?;
    file.write_all_at(b"hello\n".to_vec(), 0).await.0?;
    file.sync_all().await?;
    let text = fs::read("log.txt").await?;
    assert_eq!(text, b"hello\n");
    fs::rename("log.txt", "log.old").await
}
```

For a latency-sensitive service on dedicated cores:

```rust,no_run
fn main() -> std::io::Result<()> {
    let runtime = giong::Builder::new()
        .spin_before_park(std::time::Duration::from_micros(200))
        .build()?;
    runtime.run(|_worker| async { /* ... */ })
}
```

## Run in Docker

giong needs nothing in the image besides the program, and a container needs no special
settings: where the default seccomp profile blocks io_uring, every worker runs on epoll by
itself and says so once on stderr. A multi-stage build:

```dockerfile
FROM rust:1 AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/hello /hello
EXPOSE 8080
ENTRYPOINT ["/hello"]
```

What matters at run time:

- **Stopping.** `docker stop` and Kubernetes send `SIGTERM`. Build the runtime with
  `.shutdown_on_signals(true)` (and `.shutdown_grace(..)` to answer the requests in
  flight); a program that runs as process 1 without it ignores `SIGTERM` until it is
  killed.
- **Workers follow the CPU limit.** giong starts one worker per CPU the container may use:
  the cpuset, lowered by a CPU quota (`--cpus`, `resources.limits.cpu`), rounded down.
- **io_uring for full speed.** Docker's and Kubernetes' default profiles block it.
  [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) builds a seccomp profile that is Docker's default
  plus the three io_uring calls, for `docker run`, Docker Compose and Kubernetes, and the locked-memory
  limit (`--ulimit memlock=-1:-1`) some kernels need. `giong::driver_choice()` says which
  driver a worker got and why; log it at startup.
- **Check before you rely on it**: `GIONG_DRIVER=io_uring` makes the runtime refuse to start
  where io_uring is blocked ([GIONG-E0001](docs/errors/E0001.md)) instead of falling back.

## What is in the box

| Area | API |
|---|---|
| Starting | `giong::block_on(fut)` for one thread; `Builder::new().workers(n).build()?.run(\|i\| async { .. })` for one worker per core |
| Configuration | `.workers(n)`, `.driver(DriverKind)`, `.pin_cores(bool)`, `.spin_before_park(duration)`, `.ring_entries(n)`, `.recv_buffers(count, size)`, `.shutdown_on_signals(bool)`, `.shutdown_grace(duration)`; `giong::driver_kind()` |
| Tasks | `spawn(fut) -> JoinHandle<T>`, `handle.abort()`, `yield_now()`; futures need not be `Send` |
| TCP | `TcpListener::bind` / `bind_balanced` (busy workers hand new connections to idle ones) / `accept`, on an IP address (a host name is refused: `lookup_host` first), shared by the workers of one process and refused to any other; `TcpStream::connect` (host names resolved without blocking, Happy Eyeballs) / `recv` / `read` / `write` / `write_all` / `write_all_vectored` / `shutdown` / `set_nodelay` |
| Names | `net::lookup_host("host:port")`; `net::Resolver` (`/etc/hosts`, then DNS over UDP and TCP, search list, cache) |
| Unix sockets | `UnixListener::bind` / `accept`; `UnixStream::connect` / `pair`, with the same read and write methods as `TcpStream` |
| UDP | `UdpSocket::bind` / `connect` (IP addresses); `recv_from` / `recv` (multishot, pool buffer); `read_from` / `read` (your buffer); `send_to` / `send`; `set_broadcast`; with the `unstable-udp-offload` feature, GSO (`send_segments_to`), GRO (`set_gro`), ECN and the local address of a datagram |
| Files | `fs::File::open` / `create` / `options`; `read_at` / `write_at` / `write_all_at` / `sync_all` / `sync_data` / `allocate` / `metadata` / `len` / `close`; `fs::read`, `write`, `metadata`, `rename`, `remove_file`, `create_dir`, `remove_dir`, `read_dir` |
| Time | `time::sleep`, `sleep_until`, `timeout(duration, fut)` |
| Shutdown | `shutdown_signal()`, `until_shutdown(fut)`, `is_shutting_down()`, `Handle::shutdown()` |
| Across threads | `Handle::spawn_on(worker, \|\| async { .. }) -> RemoteJoinHandle<T>` (await or abort it from any thread), `Handle::spawn_balanced(\|\| async { .. })` (the first idle worker starts it), `Handle::least_loaded_worker()`, `sync::mpsc::unbounded()`, `sync::mpsc::bounded(n)` (senders wait when full), `sync::oneshot::channel()` (one value) |
| Errors | Every error giong raises starts with a code (`GIONG-E0102: no address found for …`) and links to its page in [docs/errors](docs/errors/README.md); `error::code(&err)` returns it as a `Code` to match on |
| HTTP | [`giong-http`](giong-http/README.md): `Server::serve` / `serve_unix` / `serve_tls`, `Request`, `Response`, `BodySender` and `Trailers` for streamed bodies; with the `client` feature, `client::Client` (`get`, `post`, ... `send`, `preconnect`, `ping_idle_http2`), `client::Response`; with `unstable-http3`, `Server::serve_h3` and `ClientBuilder::http3` (Rust 1.85) |
| gRPC | [`giong-grpc`](giong-grpc/README.md): `Server`, `Service`, `Channel`, `Status`, `Metadata`; `health::Health`, `reflection::Reflection`; code generated by [`giong-grpc-build`](giong-grpc-build/README.md) in `build.rs` (no `protoc`) |
| Protocol Buffers | [`giong-proto`](giong-proto/README.md): generated `…View::decode(&buf)?` views that borrow from the input; message structs and `Encode` for writing |
| QUIC | [`giong-quic`](giong-quic/README.md) (Rust 1.85): `Endpoint::server` / `client`, `Connection`, `SendStream`, `RecvStream`, 0-RTT |
| TLS | [`giong-tls`](giong-tls/README.md): `TlsAcceptor`, `TlsConnector`, `TlsStream` |
| PostgreSQL | [`giong-postgres`](giong-postgres/README.md): `Client`, `Pool`, `Config`, `Row`, `Transaction` |
| MySQL, MariaDB | [`giong-mysql`](giong-mysql/README.md): `Client`, `Pool`, `Config`, `Row`, `Transaction` |
| Redis | [`giong-redis`](giong-redis/README.md): `Client`, `Pool`, `Config`, `cmd`, `Pipeline`, `PubSub` |
| Web services | [`giong-web`](giong-web/README.md): `Router`, `App`, `Req`, `Error`, `Middleware`; `middleware::{RequestId, Logger, Cors, Compression, Timeout}`; `json` with `serde` |
| Tokio-ecosystem crates | [`giong-compat`](giong-compat/README.md): `Compat::new(stream)` implements `AsyncRead`, `AsyncWrite` and `AsyncBufRead` from `tokio::io` and `futures_io` for `TcpStream`, `UnixStream` and `TlsStream`, for crates that take a stream and are driven by you (h2, tokio-rustls, tokio-tungstenite, hyper's `conn`, tokio-util's codecs) |
| Looking inside | `Handle::pulse(worker)` (counters and the task being polled, from any thread), `observe::snapshot()` (live tasks by the line that spawned them, timers, I/O, pools), `Handle::set_profiling` and `observe::take_profile()`, `observe::worker_stats()` (the current worker's counters, for leak checks in tests); [`giong-mcp`](giong-mcp/README.md) serves them to AI agents over MCP |

Five things to remember:

1. `recv()` returns a `RecvBuf` borrowed from the worker's pool. **An empty buffer means
   the peer closed the connection.** Copy what you need and drop it; do not hold it across
   an `.await`.
2. `recv()`, `recv_from()` and `accept()` are cancel safe: a `timeout` around them loses no
   data, datagram or connection. The one-off operations (`read`, `read_from`, `read_at` on
   a pipe) are not: one that is dropped mid-flight can still take data that arrives before
   the kernel has processed the cancellation.
3. A datagram received with `recv_from()` lives in a pool buffer, so its longest payload is
   the buffer size minus 44 bytes (16 KiB − 44 by default). Longer ones are cut off and
   `is_truncated()` says so; raise `.recv_buffers(count, size)` or use `read_from()` with a
   buffer of your own. Do not mix the two kinds on one socket.
4. A `File` has no cursor: every read and write names its offset. Opened with
   `append(true)`, writes go to the end whatever the offset.
5. Timers have 1 ms resolution and never fire early.

## Choosing crates

For each job a service has, try these in order and take the first that fits:

1. **A giong crate.** Each one was written for giong: it reads into the worker's buffer
   pool, hands owned buffers to the kernel, and batches system calls (many requests per
   read and many answers per write on a connection, and the statements of every task
   pipelined over a pool's few connections). Most were measured against the Tokio crate
   they replace, with the factors in the table below.
2. **A crate that needs no runtime.** serde, rustls, `futures`' combinators, Tokio's
   `sync` module (`Semaphore`, `Notify`, `watch`, `broadcast`), and anything else built
   only on `std::future`, run on giong as they are.
3. **A Tokio-ecosystem crate that takes a stream from you**, over
   [`giong_compat::Compat`](giong-compat/README.md): h2, tokio-rustls, tokio-tungstenite,
   hyper's `conn` API, tokio-util's codecs. This is the bridge for a protocol giong has no
   crate for yet (WebSocket, for one), not a second way to do what a giong crate does.
4. **A crate that needs the Tokio runtime** (reqwest, sqlx, tonic's `transport`,
   redis-rs, most cloud SDKs) does not run on giong's workers: it panics with "there is no
   reactor running", or never finishes. Use the giong crate for the job if there is one;
   otherwise run the crate on a Tokio runtime in a thread of its own and pass work to it
   over a channel.

| You need | Use | Instead of | Measured against it (one core each, medians, method under [Benchmarks](#benchmarks)) |
|---|---|---|---|
| An HTTP/1.1 server | [`giong-http`](giong-http/README.md) | hyper, axum | 1.2 to 1.6 times axum on `/json` (1.2 where the load generator was the limit), 2.8 to 3.2 times on 8 KB answers ([Throughput](docs/BENCHMARKS.md#throughput-at-50-connections), and the plain rows under [HTTPS](docs/BENCHMARKS.md#https)) |
| An HTTP/2 server | `giong-http` (on by default) | hyper, axum | 3.8 to 4.4 times axum on small answers, 2.5 to 3.0 times on 8.5 KiB ones ([giong-http](giong-http/README.md)) |
| Routing, JSON, middleware | [`giong-web`](giong-web/README.md) | axum, tower-http | 1.3 times axum on `/json` and 1.8 times on 8 KB answers; with request ids, gzip and a timeout, 1.9 and 2.3 times ([Routing](docs/BENCHMARKS.md#routing-json-and-middleware)) |
| An HTTP client | `giong-http`, feature `client` | reqwest, hyper's client | 1.6 to 1.8 times reqwest over HTTP/1.1; over HTTP/2 2.0 to 2.2 times on small answers, 1.2 to 1.3 times on 8.5 KiB ones ([giong-http](giong-http/README.md)) |
| TLS | [`giong-tls`](giong-tls/README.md) | tokio-rustls | The same rustls underneath. Measured with `giong-http`: 1.8 times axum + rustls on `/json`, 2.6 times on 8 KB answers, 1.18 times the new connections per second (level with axum set to prefer AES-128-GCM as giong does) ([HTTPS](docs/BENCHMARKS.md#https)) |
| PostgreSQL | [`giong-postgres`](giong-postgres/README.md) | tokio-postgres, sqlx | 3.3 times tokio-postgres + deadpool at its best on one row by key, 2.5 times on 100 rows; 7–9% behind tokio-postgres on one shared connection ([PostgreSQL](docs/BENCHMARKS.md#postgresql)) |
| MySQL, MariaDB | [`giong-mysql`](giong-mysql/README.md) | sqlx, mysql_async | 5.1 times SQLx at its best on one row by key, 3.7 times on 100 rows ([MySQL](docs/BENCHMARKS.md#mysql)) |
| Redis | [`giong-redis`](giong-redis/README.md) | redis-rs, fred | 1.1 to 1.8 times redis-rs on small commands, 1.25 to 2.5 times on 8 KiB values, at half its CPU per command or less ([Redis](docs/BENCHMARKS.md#redis)) |
| Files | `giong::fs` | `tokio::fs` | 8 times Tokio on reads at an offset, 3.2 to 4.1 times on whole small files ([File I/O](docs/BENCHMARKS.md#file-io)) |
| Name lookups | `giong::net::lookup_host`, `net::Resolver` | `tokio::net::lookup_host`, hickory-resolver | `localhost` from `/etc/hosts`: 171 ns against Tokio's 28.6 µs ([Core primitives](docs/BENCHMARKS.md#core-primitives)); DNS over the network not measured |
| Tasks, timers, channels | `giong::spawn`, `giong::time`, `giong::sync` (`mpsc`, `oneshot`) | `tokio::spawn`, `tokio::time`, `tokio::sync` | 1.3 to 2.6 times Tokio; Monoio and Glommio are ahead on spawning and channels within one thread ([Core primitives](docs/BENCHMARKS.md#core-primitives)) |
| A gRPC server or client | [`giong-grpc`](giong-grpc/README.md), with [`giong-grpc-build`](giong-grpc-build/README.md) | tonic, prost | 1.3 to 1.5 times tonic on 100-byte unary calls, 1.2 to 1.25 times on 10 kB ones, 3.8 to 4.0 times on server streams, half the memory per open stream; a one-thread client level to 14% ahead ([giong-grpc](giong-grpc/README.md#measured-against-tonic)) |
| Protocol Buffers | [`giong-proto`](giong-proto/README.md) | prost | Views over the receive buffer, no allocation per field; not measured on its own against prost |
| HTTP/3, QUIC | `giong-http`, feature `unstable-http3`; [`giong-quic`](giong-quic/README.md) | quinn, h3 | The same QUIC engine (quinn-proto) on giong's UDP sockets: level with it on Tokio, 18% less CPU per request than quinn ([QUIC](docs/BENCHMARKS.md#quic)); HTTP/3 end to end not measured yet |
| Runtime state for an AI agent | [`giong-mcp`](giong-mcp/README.md) | | |

Where a giong crate is not ahead, the sections linked above say so: over one shared
connection giong-postgres is 7–9% below tokio-postgres at the median, a blocking `std`
read of a whole small cached file beats every async one, Monoio and Glommio spawn tasks and
pass values within one thread faster, and TLS handshakes cost the same on both once axum's
rustls is set to the cipher suite giong-tls prefers.

What `Compat` adds is measured on its own, on one TCP stream with no protocol on top
([giong-compat](giong-compat/README.md#what-it-costs)): large writes go straight from the
caller's memory to the socket, from 9% above to 6% below Tokio's CPU per byte sent over the
last two sessions (until 0.8.0 they were copied first: 25% above Tokio in a session the same
day); against giong's own `recv`, 6–16% more CPU per byte read with `read`, and no measurable
difference through `AsyncBufRead`; up to 5% more CPU per small request and reply, and level
with Tokio there. Receiving costs more CPU per byte than Tokio's own (14% through
`AsyncBufRead`, 19% with `read`, in the latest session, with 16 KiB receive buffers; 17% and
30% with 8 KiB), most of it in giong's own receive (12% above Tokio).
A protocol crate over `Compat` brings its own design on top of that, and none has been
measured against the giong crate for the same protocol. The reason to prefer the giong
crate is its design (step 1) and the gaps in the table, measured against these Tokio
crates on Tokio: running one of them on giong through `Compat` does not change how it
reads, writes or pipelines, and adds the copies above.

To tell steps 3 and 4 apart: a crate of step 3 takes a stream you made
(`S: AsyncRead + AsyncWrite`) and gives back a future or a connection object for your task
to poll. A crate of step 4 opens its own sockets (`connect(url)`), sleeps or spawns:
`cargo tree -e features -i tokio` shows it turning on Tokio's `net`, `time`, `rt` or
`process` feature. Running it once settles it.

## Coming from Tokio

giong has no macros and no `spawn_blocking`, and a worker's state is never shared with
another thread, so some everyday Tokio code is written another way. Every row was run on
giong 0.7.0.

| With Tokio | On giong |
|---|---|
| `#[tokio::main]` | `giong::block_on(fut)` for one thread; `Builder::new().workers(n).build()?.run(\|i\| async { .. })` runs the closure's future on every worker |
| `tokio::spawn(fut)` | `giong::spawn(fut)`: the task stays on the worker that spawned it, so the future need not be `Send`. From another thread or for another worker: `Handle::spawn_on`, `Handle::spawn_balanced` |
| `JoinHandle`, `abort()` | the same: `handle.await` is `Err` for a task that panicked or was aborted |
| `tokio::join!`, `tokio::select!` | `futures::join!` and `futures::select!` from the `futures` crate, which need no runtime |
| `spawn_blocking(f).await` | nothing built in yet: `std::thread::spawn`, and a `giong::sync::oneshot` channel to await the result. A worker that blocks holds up every connection it owns |
| `Arc<Mutex<T>>` shared by handlers | state per worker in an `Rc<RefCell<T>>`, built inside `run(\|_\| async { .. })` and not borrowed across an `.await`. What every worker must see goes through atomics or a channel |
| `tokio::sync::{Semaphore, Notify, watch, broadcast, Mutex}` | `giong::sync` has `mpsc` (unbounded and bounded) and `oneshot`. Tokio's `sync` module needs no Tokio runtime and works on giong as it is: `tokio = { version = "1", default-features = false, features = ["sync"] }` |
| `tokio::time::{sleep, sleep_until, timeout}` | `giong::time::{sleep, sleep_until, timeout}` |
| `tokio::time::interval` | a loop around `sleep_until(next)` |
| `tokio::signal::ctrl_c()` | `Builder::shutdown_on_signals(true)`, then `giong::shutdown_signal().await`, or `giong::until_shutdown(fut)` around a wait |
| `stream.read(&mut buf)`, `stream.write_all(&buf)` | `stream.recv().await?` lends a buffer from the worker's pool; a write takes its buffer by value and hands it back: `let (result, buf) = stream.write_all(buf).await;` |
| tonic and prost | `giong-grpc` with `giong-grpc-build` in `build.rs` (no `protoc`), messages as `giong-proto` views |
| quinn, h3 | `giong-quic`; giong-http's `unstable-http3` feature for HTTP/3 |
| sqlx, tokio-postgres, redis-rs, reqwest, axum | `giong-postgres`, `giong-mysql`, `giong-redis`, `giong-http`'s client, `giong-web`. Crates that are handed a stream (h2, tokio-rustls, tokio-tungstenite) run through `giong_compat::Compat::new(stream)`; see [Choosing crates](#choosing-crates) |

## Design

```text
        worker 0                 worker 1                 worker N
 ┌────────────────────┐   ┌────────────────────┐   ┌────────────────────┐
 │ tasks (!Send)      │   │                    │   │                    │
 │ run queue          │   │        ...         │   │        ...         │
 │ timer wheel        │   │                    │   │                    │
 │ receive buffers    │   │                    │   │                    │
 │ its own io_uring   │   │                    │   │                    │
 └─────────┬──────────┘   └─────────┬──────────┘   └─────────┬──────────┘
           │   eventfd + injector (only when another thread wakes a task)  │
           └──────────────────────────┴────────────────────────────────────┘
```

Each worker is a thread pinned to one core that owns everything it touches. Its loop:

```text
run the tasks that are ready  →  io_uring_enter (submit everything, collect everything)  →  fire timers  →  repeat
```

| Part | How | Why |
|---|---|---|
| **Threads** | Thread-per-core, shared-nothing. A task never changes thread. | Futures need not be `Send`; `Rc` and `RefCell` just work. No locks on the hot path. |
| **Wakers** | A waker does not own its task; it holds a slot number and a handle to the worker. Same thread: push onto the local queue. Other thread: injector plus eventfd. | Wakers must be `Send + Sync` although tasks are not. This gets there without `unsafe`. |
| **I/O** | Everything a tick produces enters the kernel in one `io_uring_enter`, with `DEFER_TASKRUN` and `SINGLE_ISSUER`, through the thread's registered ring descriptor (5.18+). | The fewest system calls possible; completions are processed only when the worker asks. |
| **Accept and receive** | Multishot accept; multishot receive with a provided buffer ring, for TCP, Unix sockets and UDP (`recvmsg`). On Linux 6.12+ streams fill each buffer piece by piece (`IOU_PBUF_RING_INC`). | One submission for the life of a listener, connection or socket. The kernel picks a buffer only when data arrives, so the buffer is warm in cache and idle connections hold none; connections that send a little share a buffer. |
| **Writes and files** | Ownership passing: `write_all(buf)` returns `(result, buf)`. TCP sends of 16 KiB and more from bytes that cannot change (`&'static`, `Rc<[u8]>`, `Arc<[u8]>`) go out without a copy on 6.2+ (`SEND_ZC`), and the write still returns as soon as they are queued. File operations are ordinary entries in the same ring. | The kernel keeps a pointer to the buffer until it is done; this API makes that sound in Rust. Files need no pool of blocking threads. |
| **Cancellation** | When a future is dropped mid-flight, its buffer moves to the driver, a cancel is sent, and the memory is freed only when the kernel confirms. | Avoids use-after-free, the classic bug of completion-based runtimes. |
| **Backpressure** | A reader more than an eighth of the pool behind has its multishot receive paused (TCP then slows the sender), and its backlog copied off the pool while the pool runs low. A dry pool reclaims buffers that hold data left unread for a whole turn of the event loop. An accept backlog over 1,024 pauses the accept. | One stalled connection cannot starve the rest, and a busy one that keeps up is never copied. |
| **Timers** | A hierarchical timing wheel in user space (six levels of 64 slots, 1 ms ticks). | Arming and cancelling are O(1) and make no system call. Nearly every timer is an I/O timeout that is cancelled before it fires. |
| **Latency dial** | Optionally busy-poll the ring for a while before sleeping. | A sleeping thread must be woken by an interrupt and rescheduled, which costs tens of microseconds, more in a virtual machine. |
| **Panics** | A panic in a task is caught and reported through its `JoinHandle`. | One bad connection does not take down the worker. |
| **Shutdown** | With a grace period, a worker keeps running its tasks after its main future returns, until they finish or the period ends. Then: cancel everything, wait for the kernel to confirm, free. If confirmation does not come, leak on purpose. | Requests in progress are answered. Memory safety matters more than a few megabytes at exit. |
| **Old kernels** | Three ring setups, tried in order: `DEFER_TASKRUN` (6.1+), `COOP_TASKRUN` (6.0), no flags. | The same code runs on 6.0; the two fallbacks are tested on every change by forcing them. |
| **No io_uring** | An epoll driver behind the same operations, chosen when the ring cannot be set up. | Containers and sandboxes that block io_uring still run giong (see [Drivers](#drivers)). |

## Drivers

Each worker runs on one I/O driver, behind the same API. Linux has two:

- **io_uring**, the default, which everything above describes. It needs Linux 6.0 or newer
  and the three io_uring system calls.
- **epoll**, chosen without being told where io_uring cannot be set up: a Docker container
  with the default seccomp profile, Kubernetes with `RuntimeDefault`, gVisor, the
  `kernel.io_uring_disabled` sysctl, or a kernel older than 6.0 (tested on 5.15).
  [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) shows how to allow io_uring where it is blocked.

`Builder::driver(DriverKind::IoUring)`, or `GIONG_DRIVER=io_uring`, makes the runtime fail
to start where io_uring is missing ([GIONG-E0001](docs/errors/E0001.md)) instead of falling
back; `DriverKind::Epoll`, or `GIONG_DRIVER=epoll`, chooses epoll anywhere. A fallback
is said once on stderr, with the reason, and `giong::driver_choice()` tells which driver
a worker got, who chose it and why. Kernel bugs that would cost io_uring are worked
around: Ubuntu 24.04's 6.8 kernels from 6.8.0-135 on refuse correctly formed buffer rings,
and giong registers its ring the way they accept ([DEPLOYMENT.md](docs/DEPLOYMENT.md#ubuntu-2404)).
The whole test suite runs on both drivers, and in CI also inside a Docker container with
the default profile, on a real 5.15 kernel and on Ubuntu 24.04's own kernel; CI fails if
giong would not choose io_uring where it should.

| | io_uring | epoll |
|---|---|---|
| A socket read or write | queued, and handed to the kernel with the rest of the tick in one `io_uring_enter` | tried at once with a non-blocking system call; on `EAGAIN` the task waits until the socket is ready |
| Accept and receive | multishot, into a buffer the kernel picks | tried when the socket becomes readable, into the same pool of buffers |
| Registering a socket | nothing to register | once, edge-triggered, when it is first used |
| Waiting | `io_uring_enter` | `epoll_pwait2` |
| Files | entries in the same ring | a pool of blocking threads; pipes and sockets opened as files use readiness |
| Cancelling a file read or write | the kernel cancels it | one already running on a thread finishes there, and its buffer is freed then |
| A task whose I/O never waits | yields at the end of the tick | yields after 128 operations, so it cannot starve the others |

On epoll giong is an epoll runtime like the others and as fast as they are (two system
calls per request, the same requests per second as raw Tokio); what sets it apart comes
from io_uring, such as 70% more requests per second on 8 KB responses. The measured table
is under [io_uring against epoll](docs/BENCHMARKS.md#io_uring-against-epoll).

The other systems have one driver each, chosen by default (`DriverKind` is
`non_exhaustive`; asking for a driver the system does not have fails with
[GIONG-E0004](docs/errors/E0004.md)):

- **kqueue on macOS** (`DriverKind::Kqueue`): the readiness loop of the epoll driver, files
  on the pool of blocking threads, a wake-up through `EVFILT_USER`. The workers of an
  address share one socket bound without `SO_REUSEPORT`, so no other process can take the
  port.
- **I/O completion ports on Windows** (`DriverKind::Iocp`, `GIONG_DRIVER=iocp`): operations
  complete the way they do on io_uring, so buffers are handed over and handed back the same
  way. Multishot accept is `AcceptEx` into a socket made ahead; an idle connection waits
  with a zero-byte receive and holds no buffer; a dropped operation is cancelled with
  `CancelIoEx` and its buffer kept until the kernel lets go of it. Windows has no
  `SO_REUSEPORT`, so the workers of an address share one socket: connections are handed to
  them in turn, datagrams by a hash of the sender's address.

## Benchmarks

Every table, with its method, machine and raw output, is in
[docs/BENCHMARKS.md](docs/BENCHMARKS.md): throughput from one connection to 5,000, memory
per connection, system calls, bare metal from one worker to sixteen, HTTPS, the database and
Redis clients, routing, files, core primitives, UDP, gRPC, large uploads, QUIC, the
benchmark lab against Tokio, Monoio and Glommio, and what has not been measured. A change of
a percent or two is below the noise of those runs, so CI holds the hot paths to their
instruction counts instead ([bench/instr](bench/instr/README.md)): a change that makes any
of them cost 1% more fails.

## Testing and safety

More than 1,400 tests across the thirteen crates and the examples (`cargo test --workspace
--all-features`, documentation examples included), run in release mode on all three
io_uring setups and on the epoll driver without a failure, every crate's on macOS and the
runtime's on Windows. The Rust samples of the READMEs are compiled (and run where they need
no server) as doctests of the example API. The runtime's, HTTP's and TLS's main suites are
below; the other crates and the example API describe theirs in their READMEs.

| Suite | What it checks |
|---|---|
| `tests/executor.rs` (16) | spawn and join, panics, abort, teardown, runtimes run back to back; I/O completes while another task keeps yielding; giong called outside a runtime, or `block_on` inside one, panics with its code |
| `tests/time.rs` (8), `src/time.rs` (4) | sleep, timeout, ordering, 5,000 timers; the timing wheel against a reference model over 87 random operation sequences with timers from 1 ms to 30 days |
| `tests/net.rs` (30) | echo, 8 MB each way without a wrong byte, EOF, 6 MB in turns of `recv` and receives straight into the reader's buffer without a byte out of place, 500 and 1,300 connections, plain `std` clients; writes from 16 KiB − 1 to 1 MiB + 3, plain and vectored, intact with and without zero-copy send; a bulk transfer to a reader that keeps up is not copied; `send_file` of 9 MB and of ranges, past a file's end and to a closed peer; **a non-blocking socket under io_uring never reports `WouldBlock`** (it did on Linux 6.0 and 6.1); binding a busy address fails (Linux 6.0.16 to 6.0.19 report success and leave the socket unbound), and so does a port another process shares through `SO_REUSEPORT`, while a dropped listener's port binds again at once; a host name is refused with what to write instead |
| `tests/unix.rs` (8) | echo over a socket file, 4 MB each way over `pair()`, bind and connect errors, 300 connections, `std` interop |
| `tests/udp.rs` (25) | IPv4 and IPv6, connected sockets, **a cancelled `recv_from` loses no datagram**, many tasks receiving on one socket each woken for a datagram of its own, a receiver that gave up not taking the next datagram, a task that only sends letting the others run, a burst of 64 datagrams complete and in order, truncation, 2,000 datagrams echoed through one buffer, a pool of 4 buffers not starved by one socket, three workers on one port, 1,000 sockets on port 0 each with a port of its own, sockets closed on drop, a buffer with no room refused without losing the datagram, a port held by another process refused while sockets of one process share it, a host name refused by `bind` and `connect` |
| `tests/fs.rs` (25) | files intact up to 8 MB, positional reads and writes, open options like `std`, metadata, rename and remove, 200 tasks sharing one file, descriptors closed exactly once, an operation cancelled in flight, a file closed only after the blocking thread using it is done (epoll), one stuck operation not holding up the others, offsets above `i64::MAX` and open options that cannot work refused with their codes, a pipe on standard input left blocking |
| `tests/cancel.rs` (17) | **a buffer stays alive until the kernel confirms the cancellation**; 4 buffers for 32 connections; a stalled connection cannot starve another; a dropped `send_file` leaves nothing behind; **a dropped open closes the descriptor it produced**; a stream or operation dropped by code the driver runs is still closed and cancelled; connections accepted for a listener that outlives its runtime are closed; teardown with everything in flight |
| `tests/multi.rs` (16) | several workers, cross-thread wake-ups, 30,000 channel messages, `SO_REUSEPORT`, SIGTERM (also after worker 0 returned), work sent to a stopped runtime cancelled, a panicking worker |
| `tests/shutdown.rs` (10) | **graceful shutdown**: remaining tasks finish within the grace period and are dropped when it expires; requests in progress are answered, idle connections close at once |
| `tests/spin.rs` (6) | the busy-poll mode behaves like the default: I/O, timers, cross-thread wake-ups, shutdown |
| `tests/driver.rs` (9), `tests/blocked.rs` (4) | choosing a driver in code or with `GIONG_DRIVER`; **with io_uring blocked by a seccomp filter, a runtime starts on epoll by itself**, and fails with GIONG-E0001 when io_uring was asked for; with the locked-memory limit used up, the same with GIONG-E0010; the optional io_uring features found are the kernel's; a task whose I/O never waits still lets the others run; a socket that gets the number of one whose connect failed is still woken |
| `tests/observe.rs` (10) | tasks named by the line that spawned them (`spawn`, `spawn_on`, `spawn_balanced`); **a pulse read from another thread names the task a blocked worker is stuck in**; counters and idle time, which counts a sleep in progress and never goes down; poll profiles by spawn location; sources reported until dropped |
| `giong-http` (215) | HTTP/1.1: 22 client tests on real sockets (keep-alive, HTTPS, redirects, timeouts, truncated bodies, gzip, streamed bodies, proxies); 27 end-to-end server tests on real sockets (pipelining, large bodies, file bodies, a request one byte at a time, 408/413/431, malformed requests, a panicking handler, graceful shutdown, a Unix socket, the client address, the connection limit, the memory budget for requests being received); 14 of bodies streamed both ways; 12 over HTTPS with a rustls client. HTTP/2: 41 unit tests of the server's streams, flow control, memory budgets, HPACK and frames; 15 client tests against the server and against servers that misbehave on purpose (refused and reset streams, overdrawn windows, malformed responses, over kernel TLS). 8 fuzz-style tests: 500,000 random inputs for the parser and the chunked decoder, HPACK header blocks, HTTP/2 connections from each side, compressed bodies, response heads and 100,000 responses read by the client in random pieces; 61 other unit tests; 15 documentation examples |
| `giong-tls` (96) | certificates chosen by SNI and replaced while serving, client certificates required or requested on the server; 15 client tests (system and built-in roots, client certificates, name and IP checks); TLS 1.2 and 1.3 with 4 MB each way, ALPN, a `recv` cancelled over and over, two and three tasks sending on one connection with every write arriving whole, **a TLS 1.2 write past the record limit failing instead of vanishing**, a write cancelled half way (the client sees a cut-off stream, never a corrupt record), a file read and encrypted, broken or stalled handshakes; 32 kernel TLS tests (every cipher suite through the kernel in both roles, files sent with `sendfile`, key updates both ways and with the last handshake flight, **the TLS 1.2 nonces read off the wire**, tampered and unexpected records, a peer that cuts its records to 32 bytes, cancellation), run on Linux 5.15, 6.1, Ubuntu's 6.8 and 6.18 with the `tls` module |
| Other | DNS, channels, slab, the io_uring setup tiers, error pages, documentation examples |

Also:

- **ThreadSanitizer** (`scripts/check.sh tsan`, Rust nightly) over the workspace's tests,
  on both drivers: no data race. Two things are handled specially, both because of the tool:
  sanitizer builds close descriptors with `close(2)`, after handing queued operations to
  the kernel (it cannot see a close that goes through the ring and then reports races on
  reused descriptor numbers), and the SIGTERM
  test is skipped (the sanitizer delivers signals only inside libc calls it intercepts,
  and giong's threads wait in raw system calls).
- **The fallback io_uring setups and the epoll driver** (`scripts/check.sh fallback`): the
  whole suite again without the optional io_uring features of newer kernels
  (`GIONG_IO_URING_OFF=all`, as on 6.1 to 6.11), with `GIONG_RING_TIER=coop` (the flags of
  kernel 6.0), `basic` (no flags at all) and `GIONG_DRIVER=epoll`.
- **io_uring blocked as in a container** (`scripts/check.sh seccomp`, and the `docker` CI
  job inside Docker's default seccomp profile): the whole suite with no driver named, so
  every runtime has to fall back to epoll by itself.
- **Real 5.15, 6.0 and 6.1 kernels** (`scripts/kernels.sh`): the test binaries booted in
  QEMU on Linux 5.15.225 (epoll: its io_uring has no buffer rings), 6.0.19 and 6.1.155,
  in CI on every change. It found that a runtime could fail to start on 6.0, which pins
  one page more than the buffer ring needs; fixed.
- **loom** (`scripts/check.sh loom`): the two handshakes between a worker and the threads
  that wake its tasks (`src/notify.rs`) are model-checked under every interleaving and
  memory-ordering outcome loom explores. It found a lost wake-up: a task waiting on state
  shared without a lock could sleep forever (about 70 in a million cross-thread wake-ups
  on x86); fixed, and `tests/wake.rs` runs that race on the real CPU.
- **Coverage-guided fuzzing** (`giong-http/fuzz/`, cargo-fuzz): the request-head parser,
  the chunked decoder, and whole connections over an in-memory transport, in pieces of
  random size. 18 million inputs locally without a finding; CI fuzzes every night and keeps
  the corpus. The other crates have targets of their own (28 in all, in each crate's
  `fuzz/`): DNS answers and `resolv.conf`, HTTP/2 from both sides, the client's reading
  of a response, whole PostgreSQL, MySQL and Redis sessions against a server made of the
  fuzzer's bytes, the records kernel TLS reads, giong-web's router and giong-mcp's
  requests.
- **Miri** (`scripts/check.sh miri`): the unit tests and the tests Miri can run (files,
  tasks, channels, timers, `observe`, several workers), on the epoll driver, in CI on
  every change. It found memory the kernel writes into being reached through a pointer
  taken before its `Box` moved, in the file and UDP paths (nothing miscompiled); fixed.
- **Kani proofs** (`scripts/check.sh kani`, [docs/kani.md](docs/kani.md)): the model
  checker proves, for every input within stated bounds, that the slab's free list stays
  sound, that no receive buffer is ever both lent to the kernel and read by user space or
  lent twice (from any pool state of two buffers, so for sequences of any length), that a
  dropped operation's memory and slab entry outlive the kernel's last completion in every
  order of events, and that `KernelBox` pointers survive moves; in CI on every change. No
  bug found; each proof caught a planted one.
- **Soak** (`scripts/check.sh soak`, `tests/soak.rs`): TCP clients on every worker that
  close cleanly, abort, half-close or cancel receives, with UDP, file and cross-worker
  traffic alongside; afterwards no descriptor, memory or driver state may be left over.
  30 minutes on four workers: 39.5 million connections, resident memory flat at 15.5–15.7
  MB. A second soak (`giong-tls/tests/soak.rs`) does the same over TLS 1.3 and 1.2 with a
  `TlsStream` at both ends, with kernel TLS on half the stream ends where the `tls` module
  is loaded: clean closes, dropped connections, handshakes given up, receives cancelled and
  taken up again, writes cancelled inside a record. 30 minutes on four workers of a 4-vCPU
  virtual machine, without kernel TLS: 7.6 million connections on io_uring and 7.4 million
  on epoll, about half of them full handshakes, no unexpected error, resident memory
  between 62 and 89 MB on io_uring and between 56 and 85 MB on epoll with no growth
  (`bench/results/tls-soak/4f9083b/`). CI
  runs both for two hours every night, on the runner's kernel and on Linux 6.18 in QEMU,
  whose packed buffer rings cut stream receives wherever a shared buffer fills up; the 6.12
  and 6.18 kernel jobs soak for a minute on every change, 6.18 with kernel TLS.
- **macOS** (`macos.yml`): clippy and every crate's tests on macOS 15 and 26 (Apple
  Silicon) on every change, ThreadSanitizer over the runtime's tests on the kqueue driver,
  and two-hour soaks of the runtime and of TLS (`macos-soak.yml`).
- **Windows** (`windows-runtime.yml`): the runtime's suites in debug and release on Windows
  Server 2022 and 2025, the release run 30 times over, and a build with Rust 1.80; the
  soak test for 150 minutes, which also checks that no handle is left open.
- **HTTP/3**: `h3spec` against giong-http's HTTP/3 server on every change (85 of 86 cases
  pass and none fails; the other needs 0-RTT, which the example leaves off).
- **valgrind memcheck** over the single-threaded tests (`scripts/valgrind.sh`): no invalid
  access, no leak.
- **Load** on the example HTTP server: over 250,000 connections opened and closed, 5,000
  concurrent connections, the client killed abruptly six times. File descriptors always
  return to the idle count; memory settles and does not grow between runs.
- **Graceful shutdown under load**: SIGTERM during 40 slow requests. Without a grace
  period 156 of 200 requests were answered; with one, 400 of 400.
- **CI** runs formatting, clippy, the documentation build, a build with Rust 1.80, the
  repeated tests, the fallback setups and the epoll driver, the tests inside Docker,
  ThreadSanitizer, Miri, the Kani proofs, the loom models, the real 5.15, 6.0 and 6.1 kernels and the core
  crate with its direct dependencies at the lowest versions it allows on every change, and
  fuzzing and the soak test every night.

Not verified, and you should know it:

- Neither sanitizer nor valgrind can see the kernel writing into memory, so cancellation
  safety rests on the design and on `tests/cancel.rs`.
- valgrind 3.22 cannot run the multi-threaded tests (it treats `io_uring_enter` as
  non-blocking, so threads starve each other); ThreadSanitizer covers those.
- Every crate, the `unsafe` code and the workspace's security were reviewed before 0.6,
  and what was found is fixed there (see the [changelog](CHANGELOG.md)), but the reviewers
  were the project's AI agents, each reading code it had not written. No outside person
  has audited giong.
- `giong-tls` has had a [security review](docs/reviews/2026-11-giong-tls.md) of its own
  code and of every crate that uses it, which found and fixed two medium issues, but it was
  done by the project's AI agent, not by an independent auditor. The cryptography and the
  protocol are rustls's; giong's part is connecting rustls to the socket.

## Compared with Monoio and Glommio

| | giong 1.4 | Monoio 0.2.4 | Glommio 0.9.0 |
|---|---|---|---|
| Platforms | Linux: io_uring on ≥ 6.0, an epoll driver elsewhere; macOS (kqueue); Windows (IOCP, the runtime crate) | Linux ≥ 5.6, with an epoll/kqueue fallback (runs on macOS) | Linux ≥ 5.8, needs a raised memlock limit |
| Receiving | multishot, kernel-provided buffers | one submission per read, caller's buffer | direct `recv`, io_uring to wait |
| Accepting | multishot | one submission per accept | one submission per accept |
| Buffer API | ownership passing | ownership passing | borrowed (`AsyncRead` / `AsyncWrite`) |
| Timeout around a read | loses no data | needs the separate `cancelable_*` API | — |
| Ring defaults | `DEFER_TASKRUN` on | no flags; pass your own builder | — |
| Busy-poll option | yes | no | yes |
| Multi-core | built-in builder with graceful draining and signals | `#[monoio::main(threads = N)]` | executor pools placed by CPU topology |
| Scheduling | FIFO | FIFO | weighted queues with latency classes |
| I/O | TCP, UDP, Unix sockets, files | TCP, UDP, Unix sockets, files | TCP, UDP, Unix sockets, Direct I/O files |
| Ecosystem | its own HTTP/1.1, HTTP/2 and HTTP/3 server and client, QUIC, gRPC and Protocol Buffers, TLS for both sides, DNS, PostgreSQL, MySQL and Redis clients, a router, an MCP server | TLS, HTTP, codecs, Tokio compatibility layer | channels, semaphores, rate control |
| Size | ~28,800 lines in `src/` with all four drivers, plus ~35,600 for HTTP and TLS and ~24,700 for the database clients | ~25,000 lines | ~40,500 lines |

Monoio is the mature choice and runs in production at ByteDance; if you need
Tokio-compatible crates today, use it. Glommio is built for storage engines: Direct I/O and
priority scheduling are its strengths, not HTTP. giong's case: the receive path
(multishot with provided buffers, cancel safe without a second API), good defaults, the
latency dial, its own crates for a whole service instead of a compatibility layer, an API
held to the four rules [above](#what-giong-is-for), and code small enough to audit. It is
younger than both, and one service runs on it so far.

## Limitations

- **Windows has the runtime crate only.** In 1.4 `giong` builds and runs there (tasks,
  timers, channels, TCP, UDP, files, DNS from the network adapters, Ctrl+C); Unix sockets
  do not exist there, and giong-http, giong-tls, the database clients, giong-grpc,
  giong-quic and giong-mcp are not built or tested on Windows yet. Trust roots from the
  Windows certificate store and processor groups beyond 64 CPUs are missing too.
- **macOS is for development.** Correct and tested on every change, not tuned: files run on
  a pool of blocking threads, CPU pinning is skipped, kernel TLS and UDP segmentation
  offloads are Linux only, and the listen backlog is macOS's 128.
- **HTTP/3 and the UDP offloads may still change.** They are behind the
  `unstable-http3` and `unstable-udp-offload` features, outside the 1.x stability promise
  until they are stabilised, and giong-quic runs on quinn-proto's engine for now.
- **Name resolution reads files and DNS only.** `connect` and `lookup_host` resolve names
  without blocking, from `/etc/hosts` and the DNS servers in `/etc/resolv.conf`; other
  Name Service Switch sources (mDNS, LDAP) are not used. `bind` takes IP addresses only,
  so it never waits on a lookup. `fs::read_dir` reads each batch of
  entries with a direct system call on the worker (io_uring cannot read directories):
  quick on a local disk, a network round trip on NFS. On the epoll driver, file
  operations run on a pool of blocking threads, and one that is cancelled while running
  finishes there. Also missing: Unix datagram
  sockets, multicast, `set_len`, file locks, `O_DIRECT` with registered buffers.
- **`giong-tls` has had no independent audit** (see the
  [self-review](docs/reviews/2026-11-giong-tls.md)).
- **The Tokio ecosystem only through I/O traits.** Crates that are generic over the
  stream and driven by you (h2, tokio-rustls, tokio-tungstenite, hyper's connection API,
  tokio-util's codecs) run on giong through [`giong-compat`](giong-compat/README.md), at
  the cost of a copy. Crates that need the Tokio runtime itself (its reactor, timers or
  `tokio::spawn`, as in sqlx, reqwest or tonic's `transport`) do not. Crates that only use
  `std::future` do. For PostgreSQL, use [`giong-postgres`](giong-postgres/README.md); for
  MySQL, [`giong-mysql`](giong-mysql/README.md); for Redis,
  [`giong-redis`](giong-redis/README.md); for HTTP requests, `giong-http`'s client.
- **Started tasks do not migrate.** Balancing happens where work starts: jobs queued with
  `Handle::spawn_balanced` go to the first idle worker, and listeners bound with
  `TcpListener::bind_balanced` hand new connections to less busy workers. A connection
  already being served stays on its worker, however busy that worker gets.
- **Graceful draining is opt-in.** By default, when a worker's main future returns, the
  remaining connections are dropped. Set `.shutdown_grace(..)` and use `until_shutdown` in
  the connection loop (`giong-http` does). Connections still in the kernel's accept queue
  when the listener closes are refused; have the load balancer stop sending first.
- **Unused so far:** fixed files, which would save 0.6% of a request, and `SQPOLL`, which
  cost more CPU than it saved on bare metal. Zero-copy send covers only bytes that cannot
  change (`&'static`, `Rc<[u8]>`, `Arc<[u8]>`); a `Vec` is copied.

## Project

- Source, issues and discussions: <https://github.com/giong-run/Giong>
- [CONTRIBUTING.md](CONTRIBUTING.md) — building, required checks, the rules for `unsafe`
  and cancellation, commit conventions, and how to make a first contribution.
- [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) — how we treat each other, and how to report a
  problem.
- [ROADMAP.md](ROADMAP.md) — milestones and deliberate non-goals.
- [AGENTS.md](AGENTS.md) — building, testing and changing giong, written for AI coding agents;
  [giong.run/llms.txt](https://giong.run/llms.txt) points a model at the docs.
- [CHANGELOG.md](CHANGELOG.md) — what changed in each release.

```text
src/
  lib.rs        entry point and docs
  runtime.rs    Builder, Runtime, Handle, the event loop, shutdown, signals
  executor.rs   tasks, wakers, spawn, JoinHandle
  notify.rs     the lock-free wake-up handshakes, checked with loom
  driver/       the drivers behind one interface (mod.rs: operations, choosing, falling back)
    uring.rs    io_uring: submit and reap, operation tracking, cancellation, backpressure
    readiness.rs  the readiness loop shared by epoll and kqueue
    epoll.rs    epoll: edge-triggered registration, the per-task budget
    kqueue.rs   kqueue, for macOS
    iocp.rs     I/O completion ports, for Windows
    blocking.rs the thread pool for file operations on the readiness drivers and Windows
  sys/          what differs between systems (linux.rs, macos.rs, unix.rs, windows.rs)
  op.rs         a single operation as a future
  buf.rs        IoBuf / IoBufMut, the kernel-shared buffer pool, RecvBuf
  kbox.rs       heap memory the kernel uses while an operation is in flight
  net.rs        the core shared by stream sockets, TcpListener, TcpStream
  net/addr.rs   ToSocketAddrs, connecting to the first of several addresses (Happy Eyeballs)
  net/dns.rs    lookup_host and Resolver: /etc/hosts, DNS over UDP and TCP, cache
  net/balance.rs  bind_balanced: handing new connections to idle workers
  net/share.rs  which ports this process holds, so bind refuses another process's
  net/spread.rs, net/udp_spread.rs  handing connections and datagrams to the workers on Windows
  net/unix.rs   UnixListener, UnixStream
  net/udp.rs    UdpSocket, Datagram
  fs.rs         File, OpenOptions, Metadata, the path operations (fs/windows.rs on Windows)
  time.rs       timing wheel, sleep, timeout
  sync.rs       mpsc channels (sync/bounded.rs: bounded), sync/oneshot.rs
  error.rs      Code, the GIONG-Exxxx codes and their pages
  observe.rs    pulses, snapshots, profiles and sources, for diagnostics and giong-mcp
giong-http/     HTTP/1.1, HTTP/2 and HTTP/3: the server (parser, chunked decoding, connections, HTTPS behind `tls`, HTTP/3 behind `unstable-http3`), the client behind `client`
giong-tls/      the TLS crate on rustls (TlsAcceptor, TlsConnector, TlsStream)
giong-postgres/ the PostgreSQL client crate (wire protocol, authentication, statement cache, Pool)
giong-mysql/    the MySQL client crate (client/server protocol, authentication, statement cache, Pool)
giong-redis/    the Redis client crate (RESP3, pipelining across tasks, Pub/Sub, Pool)
giong-web/      routing, middleware and JSON on giong-http (Router, App, Req, Error, middleware/)
giong-mcp/      the MCP server for agents: Streamable HTTP, tools over giong::observe
giong-compat/   Compat: the Tokio and futures-io I/O traits for giong's streams
giong-proto/    Protocol Buffers as views over the receive buffer (new in 1.1)
giong-grpc-build/ code generation from .proto files at build time, no protoc (new in 1.1)
giong-grpc/     gRPC servers and clients on giong-http's HTTP/2, health and reflection (new in 1.1)
giong-quic/     QUIC on giong's UDP sockets, under giong-http's HTTP/3 (new in 1.3)
examples/       echo.rs, http.rs, udp_echo.rs, cat.rs; rest-api/, a complete REST API (PostgreSQL, a Redis cache, HTTPS); proto-hello/, grpc-hello/
tests/          executor, time, net, unix, udp, fs, cancel, multi, shutdown, spin, driver, blocked, dns, channels, observe, soak, ...
bench/          run.sh, scale.sh, workers.sh, syscalls.sh, tls.sh, fs.sh, postgres.sh, mysql.sh, redis.sh, http-client.sh, web.sh, rest-api.sh, compat.sh, grpc.sh, quic.sh, lab/ (the benchmark lab), the programs compared against, results/ (tables in docs/BENCHMARKS.md)
scripts/        check.sh (repeated tests, fallback setups, epoll, seccomp, ThreadSanitizer), kernels.sh, no-io-uring.py, valgrind.sh, release.py
assets/         logo, icons, social preview (sources in assets/README.md)
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you state otherwise, any contribution you submit is licensed the
same way, without additional terms.
