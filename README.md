<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-dark.svg">
    <img alt="giong" src="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-light.svg" width="340">
  </picture>
</p>

<p align="center"><b>A thread-per-core async runtime for Linux, built on io_uring, with an epoll driver wherever io_uring is blocked.</b></p>

<p align="center">
  <a href="https://github.com/giong-run/Giong/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/giong-run/Giong/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
  <img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux-informational">
</p>

Named after Thánh Gióng, the child of Vietnamese legend who lay silent for three years and
then rose a giant: giong costs next to nothing while idle and uses every core the moment
load arrives.

> **Status: 0.4.** Complete enough to write real servers: TCP, UDP, Unix sockets, files,
> timers, multi-core, graceful shutdown, and eight companion crates:
> [`giong-http`](giong-http/README.md) (a strict HTTP/1.1 and HTTP/2 server and client),
> [`giong-web`](giong-web/README.md) (a router, middleware and JSON on it),
> [`giong-tls`](giong-tls/README.md) (TLS 1.2/1.3 on rustls), for serving HTTPS directly and connecting to TLS services,
> [`giong-postgres`](giong-postgres/README.md) (a pipelining PostgreSQL client and pool),
> [`giong-mysql`](giong-mysql/README.md) (the same for MySQL 8),
> [`giong-redis`](giong-redis/README.md) (a Redis client with Pub/Sub and a pool),
> [`giong-mcp`](giong-mcp/README.md) (an MCP server that lets AI agents look inside a
> running service)
> and [`giong-compat`](giong-compat/README.md) (the Tokio and futures-io I/O traits for
> giong's streams, so crates such as h2, tokio-rustls and tokio-tungstenite run on giong).
> The tests pass on three io_uring setups, on the epoll driver, inside Docker's default
> seccomp profile and on real 5.15, 6.0 and 6.1 kernels; ThreadSanitizer
> and valgrind are clean, loom model-checks the cross-thread wake-ups, the HTTP parser and
> HTTP/2 on both sides are fuzzed, `h2spec` passes, and a soak test churns connections for two
> hours every night. It has **not** run in production. See [Limitations](#limitations).

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
  [`giong-tls`](giong-tls/README.md) serve HTTPS at **144.8k requests/s on one core,
  against 81.0k for Axum** on the same rustls.
- **Cancellation-safe by construction.** Dropping an I/O future never frees memory the
  kernel may still write to, and wrapping `recv`, `recv_from` or `accept` in a timeout
  loses neither data nor connections.
- **Runs where io_uring cannot.** In a Docker container with the default seccomp profile,
  under gVisor or on a kernel older than 6.0, a worker switches to an epoll driver by
  itself: same API, same tests, and as fast as raw Tokio there. See
  [Two drivers](#two-drivers-io_uring-and-epoll).
- **Small enough to read.** About 8,200 lines of code for the runtime, both drivers
  included, three dependencies, and a comment on every `unsafe` block.

What it is **not**: faster than everything at everything. On the classic 50-connection
benchmark giong, a tuned Monoio and raw Tokio are within noise of each other, because the
kernel's TCP stack, not the runtime, is most of the cost. The numbers below say where the
design pays off and where it does not.

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

## Requirements

- **Linux.** For the io_uring driver, 6.0 or newer (6.1+ recommended) with io_uring
  allowed; developed and tested on 6.18. Elsewhere giong runs on its epoll driver, tested
  down to 5.15: in containers whose seccomp profile blocks io_uring (Docker's and
  containerd's default ones), under gVisor, or with the `kernel.io_uring_disabled` sysctl.
  [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) shows how to allow io_uring in Docker, Docker
  Compose and Kubernetes.
- Rust 1.80 or newer.
- giong does **not** build on Windows or macOS. On Windows use WSL2 (`uname -r`; run
  `wsl --update` for a 6.x kernel), a Linux virtual machine, or Docker.

## Quick start

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

```rust
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
            // Every worker binds its own SO_REUSEPORT listener on the same port;
            // the kernel spreads connections across cores.
            let listener = TcpListener::bind("0.0.0.0:8080").expect("bind");
            Server::new().serve(listener, handler).await;
        })
}
```

For HTTPS, turn on the `tls` feature and call `serve_tls` with a
[`giong_tls::TlsAcceptor`](giong-tls/README.md).

The runtime itself, without the library: a TCP echo server.

```rust
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

```rust
let runtime = giong::Builder::new()
    .spin_before_park(std::time::Duration::from_micros(200))
    .build()?;
```

## What is in the box

| Area | API |
|---|---|
| Starting | `giong::block_on(fut)` for one thread; `Builder::new().workers(n).build()?.run(\|i\| async { .. })` for one worker per core |
| Configuration | `.workers(n)`, `.driver(DriverKind)`, `.pin_cores(bool)`, `.spin_before_park(duration)`, `.ring_entries(n)`, `.recv_buffers(count, size)`, `.shutdown_on_signals(bool)`, `.shutdown_grace(duration)`; `giong::driver_kind()` |
| Tasks | `spawn(fut) -> JoinHandle<T>`, `handle.abort()`, `yield_now()`; futures need not be `Send` |
| TCP | `TcpListener::bind` / `bind_balanced` (busy workers hand new connections to idle ones) / `accept`; `TcpStream::connect` (host names resolved without blocking, Happy Eyeballs) / `recv` / `read` / `write` / `write_all` / `write_all_vectored` / `shutdown` / `set_nodelay` |
| Names | `net::lookup_host("host:port")`; `net::Resolver` (`/etc/hosts`, then DNS over UDP and TCP, search list, cache) |
| Unix sockets | `UnixListener::bind` / `accept`; `UnixStream::connect` / `pair`, with the same read and write methods as `TcpStream` |
| UDP | `UdpSocket::bind` / `connect`; `recv_from` / `recv` (multishot, pool buffer); `read_from` / `read` (your buffer); `send_to` / `send`; `set_broadcast` |
| Files | `fs::File::open` / `create` / `options`; `read_at` / `write_at` / `write_all_at` / `sync_all` / `sync_data` / `allocate` / `metadata` / `len` / `close`; `fs::read`, `write`, `metadata`, `rename`, `remove_file`, `create_dir`, `remove_dir`, `read_dir` |
| Time | `time::sleep`, `sleep_until`, `timeout(duration, fut)` |
| Shutdown | `shutdown_signal()`, `until_shutdown(fut)`, `is_shutting_down()`, `Handle::shutdown()` |
| Across threads | `Handle::spawn_on(worker, \|\| async { .. }) -> RemoteJoinHandle<T>` (await or abort it from any thread), `Handle::spawn_balanced(\|\| async { .. })` (the first idle worker starts it), `Handle::least_loaded_worker()`, `sync::mpsc::channel()` (unbounded), `sync::mpsc::bounded(n)` (senders wait when full), `sync::oneshot::channel()` (one value) |
| Errors | Every error giong raises starts with a code (`GIONG-E0102: no address found for …`) and links to its page in [docs/errors](docs/errors/README.md); `error::code(&err)` returns it as a `Code` to match on |
| HTTP | [`giong-http`](giong-http/README.md): `Server::serve` / `serve_unix` / `serve_tls`, `Request`, `Response`; with the `client` feature, `client::Client` (`get`, `post`, ... `send`), `client::Response` |
| TLS | [`giong-tls`](giong-tls/README.md): `TlsAcceptor`, `TlsConnector`, `TlsStream` |
| PostgreSQL | [`giong-postgres`](giong-postgres/README.md): `Client`, `Pool`, `Config`, `Row`, `Transaction` |
| MySQL | [`giong-mysql`](giong-mysql/README.md): `Client`, `Pool`, `Config`, `Row`, `Transaction` |
| Redis | [`giong-redis`](giong-redis/README.md): `Client`, `Pool`, `Config`, `cmd`, `Pipeline`, `PubSub` |
| Web services | [`giong-web`](giong-web/README.md): `Router`, `App`, `Req`, `Error`, `Middleware`; `middleware::{RequestId, Logger, Cors, Compression, Timeout}`; `json` with `serde` |
| Tokio-ecosystem crates | [`giong-compat`](giong-compat/README.md): `Compat::new(stream)` implements `AsyncRead`, `AsyncWrite` and `AsyncBufRead` from `tokio::io` and `futures_io` for `TcpStream`, `UnixStream` and `TlsStream`, for crates that take a stream and are driven by you (h2, tokio-rustls, tokio-tungstenite, hyper's `conn`, tokio-util's codecs) |
| Looking inside | `Handle::pulse(worker)` (counters and the task being polled, from any thread), `observe::snapshot()` (live tasks by the line that spawned them, timers, I/O, pools), `Handle::set_profiling` and `observe::take_profile()`; [`giong-mcp`](giong-mcp/README.md) serves them to AI agents over MCP |

Five things to remember:

1. `recv()` returns a `RecvBuf` borrowed from the worker's pool. **An empty buffer means
   the peer closed the connection.** Copy what you need and drop it; do not hold it across
   an `.await`.
2. `recv()`, `recv_from()` and `accept()` are cancel safe: a `timeout` around them loses no
   data, datagram or connection. The one-off operations (`read`, `read_from`, `read_at` on
   a pipe) are not: one that is dropped mid-flight can still take data that arrives before
   the kernel has processed the cancellation.
3. A datagram received with `recv_from()` lives in a pool buffer, so its longest payload is
   the buffer size minus 44 bytes (8 KiB − 44 by default). Longer ones are cut off and
   `is_truncated()` says so; raise `.recv_buffers(count, size)` or use `read_from()` with a
   buffer of your own. Do not mix the two kinds on one socket.
4. A `File` has no cursor: every read and write names its offset. Opened with
   `append(true)`, writes go to the end whatever the offset.
5. Timers have 1 ms resolution and never fire early.

## Design

```
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

```
run the tasks that are ready  →  io_uring_enter (submit everything, collect everything)  →  fire timers  →  repeat
```

| Part | How | Why |
|---|---|---|
| **Threads** | Thread-per-core, shared-nothing. A task never changes thread. | Futures need not be `Send`; `Rc` and `RefCell` just work. No locks on the hot path. |
| **Wakers** | A waker does not own its task; it holds a slot number and a handle to the worker. Same thread: push onto the local queue. Other thread: injector plus eventfd. | Wakers must be `Send + Sync` although tasks are not. This gets there without `unsafe`. |
| **I/O** | Everything a tick produces enters the kernel in one `io_uring_enter`, with `DEFER_TASKRUN` and `SINGLE_ISSUER`. | The fewest system calls possible; completions are processed only when the worker asks. |
| **Accept and receive** | Multishot accept; multishot receive with a provided buffer ring, for TCP, Unix sockets and UDP (`recvmsg`). On Linux 6.12+ streams fill each buffer piece by piece (`IOU_PBUF_RING_INC`). | One submission for the life of a listener, connection or socket. The kernel picks a buffer only when data arrives, so the buffer is warm in cache and idle connections hold none; connections that send a little share a buffer. |
| **Writes and files** | Ownership passing: `write_all(buf)` returns `(result, buf)`. File operations are ordinary entries in the same ring. | The kernel keeps a pointer to the buffer until it is done; this API makes that sound in Rust. Files need no pool of blocking threads. |
| **Cancellation** | When a future is dropped mid-flight, its buffer moves to the driver, a cancel is sent, and the memory is freed only when the kernel confirms. | Avoids use-after-free, the classic bug of completion-based runtimes. |
| **Backpressure** | A reader more than an eighth of the pool behind has its multishot receive paused (TCP then slows the sender), and its backlog copied off the pool while the pool runs low. A dry pool reclaims buffers that hold data left unread for a whole turn of the event loop. An accept backlog over 1,024 pauses the accept. | One stalled connection cannot starve the rest, and a busy one that keeps up is never copied. |
| **Timers** | A hierarchical timing wheel in user space (six levels of 64 slots, 1 ms ticks). | Arming and cancelling are O(1) and make no system call. Nearly every timer is an I/O timeout that is cancelled before it fires. |
| **Latency dial** | Optionally busy-poll the ring for a while before sleeping. | A sleeping thread must be woken by an interrupt and rescheduled, which costs tens of microseconds, more in a virtual machine. |
| **Panics** | A panic in a task is caught and reported through its `JoinHandle`. | One bad connection does not take down the worker. |
| **Shutdown** | With a grace period, a worker keeps running its tasks after its main future returns, until they finish or the period ends. Then: cancel everything, wait for the kernel to confirm, free. If confirmation does not come, leak on purpose. | Requests in progress are answered. Memory safety matters more than a few megabytes at exit. |
| **Old kernels** | Three ring setups, tried in order: `DEFER_TASKRUN` (6.1+), `COOP_TASKRUN` (6.0), no flags. | The same code runs on 6.0; the two fallbacks are tested on every change by forcing them. |
| **No io_uring** | An epoll driver behind the same operations, chosen when the ring cannot be set up. | Containers and sandboxes that block io_uring still run giong (see [Two drivers](#two-drivers-io_uring-and-epoll)). |

## Two drivers: io_uring and epoll

Each worker runs on one of two I/O drivers, behind the same API:

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

The same server program on both drivers (`GIONG_DRIVER`), against raw Tokio and the
hand-written epoll server from [Benchmarks](#benchmarks), on the same machine with the same
method, one core each. The first rows are `bench/run.sh` at 50 connections (medians of five
rounds), the next ones `bench/scale.sh` (three rounds) and `bench/syscalls.sh`:

| | giong, io_uring | giong, epoll | Raw Tokio | Hand-written epoll |
|---|---|---|---|---|
| `/json`: requests/s | 151.8k (141–167k) | 158.3k (156–166k) | 161.8k (158–168k) | 155.4k (148–164k) |
| `/users` (8 KB): requests/s | **55.7k** (53–59k) | 32.7k (31–35k) | 34.0k (29–36k) | 32.9k (31–35k) |
| Pipelined `/json`: requests/s | 1.43M | 1.44M | 1.43M | 1.45M |
| System calls per request | **0.04** | 2.02 | 2.05 | 2.03 |
| CPU per request, `/json` | 4.68 µs | 5.00 µs | 5.08 µs | 5.00 µs |
| CPU per request, `/users` | **18.7 µs** | 31.0 µs | 33.6 µs | 31.1 µs |
| 1 connection: median latency | 30 µs | 32 µs | 30 µs | 29 µs |
| 1,000 connections: p99 | 6.1 ms | 7.6 ms | 83.9 ms | 7.1 ms |
| 5,000 connections: requests/s | 150.0k | **152.6k** | 118.6k | 118.7k |
| 5,000 connections: CPU per request | **4.92 µs** | 5.72 µs | 6.09 µs | 5.51 µs |
| 5,000 connections: p99 | **37 ms** | 43 ms | 175 ms | 39 ms |

- **On epoll, giong is an epoll runtime like the others, and as fast as they are**: two
  system calls per request, and the same throughput as raw Tokio and the hand-written
  server on every route. At 5,000 connections it keeps the throughput of io_uring, 29%
  above the other two, at 16% more CPU per request than on io_uring.
- **What sets giong apart comes from io_uring**: a batch of responses in one system call,
  which on the 8 KB response is worth 70% more requests per second and 40% less CPU per
  request. Allow io_uring where you can.
- `giong-http` on epoll: 160.1k requests/s on `/json` against 175.6k on io_uring, and 1.05M
  against 1.28M pipelined; we have not found yet where the pipelined gap comes from.

## Benchmarks

Every server implements the same three routes (`bench/`): `/json` (a 27-byte body),
`/users` (8 KB of JSON built per request) and `/compute` (a recursive Fibonacci).
`giong`, Monoio, raw Tokio and the hand-written epoll server are deliberately the same
program, with the same minimal HTTP parser, so they compare runtimes; `giong-http` is the
library, with strict parsing, a `Date` header, timeouts and limits. Each server is pinned
to its own core and `wrk` runs on another. Servers are measured in interleaved rounds and
the tables show the median, with the range in parentheses. Machine, except for the
14-core run under [One worker to N](#one-worker-to-n): a 4-vCPU virtual machine (Xeon
2.1 GHz), Linux 6.18, Rust 1.97, Node 22, Bun 1.4, Tokio 1.54, Monoio 0.2.4, Glommio
0.9.0, wrk 4.1.

"Monoio (tuned)" is Monoio with `DEFER_TASKRUN` passed through `uring_builder`. Monoio's
default ring has no such flag and is about 20% slower on the small response; comparing
against the default would flatter giong.

### Throughput at 50 connections

| Server | `/json` | `/users` (8 KB) | `/compute` | Pipelined `/json` | Memory |
|---|---|---|---|---|---|
| Node.js (`http`) | 39.5k | 14.3k | 1.3k | 54k | 87.5 MB |
| Bun | 147.7k | 22.1k | 2.4k | 126k | 27.2 MB |
| Axum (Tokio + hyper) | 161.0k | 18.3k | 4.4k | 18k | 4.8 MB |
| Raw Tokio, own parser | 174.1k (168–179k) | 35.7k (35–38k) | 4.5k | 1.52M | 3.8 MB |
| Hand-written epoll | 170.9k (159–175k) | 37.1k (36–37k) | 4.5k | 1.61M | 2.8 MB |
| Glommio | 143.9k (133–149k) | 36.4k (34–37k) | 4.4k | 1.06M | 15.3 MB |
| Monoio (default) | 136.8k (134–143k) | 63.6k (57–65k) | 4.1k | 1.20M | 3.7 MB |
| Monoio (tuned) | 176.0k (167–182k) | 64.4k (61–65k) | 4.2k | 1.56M | 3.7 MB |
| **giong** | 173.5k (165–180k) | 63.5k (63–66k) | 4.4k | 1.54M | 5.6 MB |
| **giong-http** | 195.7k (193–213k) | 58.0k (57–58k) | 4.0k | 1.58M | 6.0 MB |

Requests per second; "Pipelined" sends 16 requests per write; "Memory" is resident memory
after the run.

- **giong, tuned Monoio, raw Tokio and epoll are within noise on `/json`.** At this rate
  `wrk` is the bottleneck: in the `syscalls.sh` run below giong's core was busy 78% of the
  time.
- **`giong-http` beats the hand-written server on `/json`, and we do not know why.** It is
  not cheaper: per request it costs 7% more CPU (4.83 against 4.51 µs in the next table),
  as checking every header and writing `Date` should. It keeps its core busier (95%), so
  it gets more out of the same load generator. Treat this row as a measurement of the
  setup, not as a reason to prefer one over the other.
- **`/users` separates the batching runtimes from the rest**: giong and Monoio at 63–64k,
  everything else at 35–37k. They send a whole batch of responses in one system call; a
  server that issues one `send` per response wakes the other vCPU each time, which is
  expensive in a virtual machine. On physical hardware expect a smaller gap. `giong-http`
  is 9% behind giong here.
- **`/compute` is the same for everyone**, as it should be: the time goes into the
  function, not the runtime. Differences of a few percent here are code-layout noise.

### From one connection to thousands

`bench/scale.sh`, `/json`. "CPU" is the server's user plus kernel time per request, read
from `/proc`. It is the number to trust when the load generator is the limit.

| Connections | | giong | giong + spin | giong-http | Monoio (tuned) | Raw Tokio | Hand-written epoll | Glommio |
|---|---|---|---|---|---|---|---|---|
| 1 | requests/s | 32.4k | **47.9k** | 30.8k | 32.3k | 31.0k | 31.9k | 27.4k |
| | median latency | 28 µs | **19 µs** | 30 µs | 28 µs | 29 µs | 29 µs | 33 µs |
| 50 | requests/s | 171.7k | **204.8k** | 198.2k | 176.1k | 169.6k | 157.2k | 137.4k |
| | CPU per request | 4.51 µs | 4.85 µs | 4.83 µs | 4.60 µs | 4.74 µs | 4.50 µs | 7.14 µs |
| 1,000 | requests/s | 176.4k | 174.6k | 178.4k | 175.3k | 165.6k | 174.3k | 173.4k |
| | CPU per request | 4.39 µs | 5.29 µs | 4.85 µs | 4.48 µs | 4.74 µs | 4.40 µs | 5.66 µs |
| | p99 latency | 5.9 ms | **5.8 ms** | 7.2 ms | 8.1 ms | 50.0 ms | 7.3 ms | 10.1 ms |
| 5,000 | requests/s | **157.9k** | 147.7k | 153.0k | 141.3k | 124.3k | 138.3k | 117.6k |
| | CPU per request | **4.42 µs** | 5.12 µs | 5.37 µs | 5.25 µs | 5.74 µs | 4.99 µs | 8.29 µs |
| | p99 latency | **26 ms** | 33 ms | 34 ms | 44 ms | 54 ms | 32 ms | 71 ms |

- **Spinning is the clearest win.** With one connection every request finds a sleeping
  server; keeping it awake cuts median latency from 28 to 19 µs and raises throughput by
  48%. It costs CPU: 20.5 instead of 13.6 µs per request, and a spinning worker keeps its
  core busy between requests.
- **At 5,000 connections giong is the cheapest per request**, 11% below hand-written epoll
  and 16% below tuned Monoio, and has the lowest p99. The likely reason is the provided
  buffer ring: receive buffers are reused while still warm in cache, where the other
  servers keep a read buffer per connection. Throughput ranges at 5,000 connections are
  wide (giong 120–162k, tuned Monoio 103–156k), so the CPU column is the one to read.
- **At 50 and 1,000 connections giong, tuned Monoio and epoll are level** on CPU per
  request (4.4–4.6 µs); raw Tokio is 5–8% more expensive and Glommio 25–60%.
- p99 is the noisiest number in the table. Raw Tokio's 50 ms at 1,000 connections is the
  median of five rounds, not a single outlier, but it is still one machine.

### Memory per connection

`bench/memory.sh`: 10,000 connections from one client, four threads per server (giong
workers, Tokio worker threads, `GOMAXPROCS`), median of three rounds, a fresh server for
each measurement. The first three columns are the server's resident memory with the
connections held, minus before, per connection. "Idle" is a keep-alive connection that
has had one request answered, the usual state of a connection from a browser, an app or
a pool; "pending" has a request whose handler is waiting (`/wait`, as for a database or
another service). "Under load" is the peak while `wrk` keeps 5,000 connections busy.
giong's two rows were measured again after the packed receive buffers below (0.5), the
others are from 0.4's run on the same kind of VM.

| Server | Open | Idle | Pending | Under load |
|---|---|---|---|---|
| **giong-http** | **1.9 KiB** | **2.0 KiB** | **6.4 KiB** | 15.7 MiB |
| **giong-http** on epoll | 2.1 KiB | 2.1 KiB | **6.4 KiB** | **10.4 MiB** |
| Axum (Tokio + hyper) | 5.7 KiB | 20.4 KiB | 19.3 KiB | 115 MiB |
| Raw Tokio, own parser | 12.9 KiB | 13.1 KiB | 12.9 KiB | 64 MiB |
| Go `net/http` | 10.8 KiB | 18.9 KiB | 18.3 KiB | 155 MiB |

- **An idle giong connection holds no buffer.** What it costs is its task (about 1.2 KB
  for giong-http's connection), the multishot receive, an idle timer and the socket. The
  kernel picks a receive buffer only when data arrives; the request and response buffers
  are taken from a per-worker pool for the length of a request. Hyper keeps an 8 KiB read
  buffer per connection, Go a 4 KiB reader and writer and a goroutine stack.
- **A handler's future is on the heap only while it runs**, in blocks the worker reuses.
  In giong 0.3 it lived inside the connection's future, and every idle connection was as
  large as the largest future its handler could make: a handler holding 8 KiB across an
  `.await` cost 43 KiB per idle connection. Each wrapper that took a future by value
  (spawn, `timeout`, `until_shutdown`) also kept two copies of it; an idle giong-http
  connection took 10.9 KiB, one with a pending request 15.0 KiB.
- **"Pending" is mostly the request buffer**: 4 KiB, held while the handler runs, because
  the `Request` it was given points into it.
- **Receive buffers are a cost per worker, not per connection**, and only as many are
  touched as the traffic needs: a worker lends the kernel 16 at first and twice as many
  each time a turn of the event loop takes most of them, and reuses the most recently
  freed first. With 20 workers and 100 busy connections, giong-http's memory rises by
  1 MiB (it rose by 40 MiB when every worker lent all 512 buffers at once).
- **Small reads share a buffer.** On Linux 6.12 and newer the kernel fills a receive
  buffer piece by piece (`IOU_PBUF_RING_INC`), so a hundred connections that each send a
  short request use one or two 8 KiB buffers rather than a hundred. Before, each receive
  took a whole buffer, and under load a worker cycled through all it had lent: with 20
  workers and 5,000 busy connections giong-http peaked 50 MiB above idle, now 23 MiB
  (epoll 15 MiB, raw Tokio 64 MiB, Axum 124 MiB, Go 184 MiB; `WORKERS=20
  SCENARIOS=load`, in `bench/results/memory-20w/`). Requests per second did not change
  (eight interleaved rounds each at 4 and 20 workers, within this VM's noise).
- **Under load io_uring still holds more than epoll**: a response's buffer stays with the
  kernel until its send completes, and received data stays in its buffer until its
  connection runs, while epoll writes at once and reads only into a buffer it needs.
  Older kernels (and `GIONG_IO_URING_OFF=packed-buffers`) take whole buffers as before.
- Throughput under load is in the other tables; here the servers and `wrk` share four
  vCPUs.

### Where the time goes

`bench/syscalls.sh`, `/json` and `/users`, 50 connections, one run:

| | System calls per request | CPU per request, `/json` (user + kernel) | CPU per request, `/users` |
|---|---|---|---|
| Raw Tokio | 2.05 | 4.92 µs (0.84 + 4.08) | 27.9 µs |
| Hand-written epoll | 2.03 | 4.49 µs (0.39 + 4.10) | 27.1 µs |
| Glommio | 2.10 | 6.89 µs (1.33 + 5.55) | 29.2 µs |
| Monoio (default) | 0.04 | 7.21 µs (0.38 + 6.83) | 15.2 µs |
| Monoio (tuned) | 0.04 | 4.64 µs (0.57 + 4.06) | 16.7 µs |
| **giong** | 0.03 | 4.42 µs (0.65 + 3.77) | 16.0 µs |
| **giong-http** | 0.03 | 4.81 µs (1.22 + 3.59) | 17.4 µs |

The system-call count is the structural difference and the most robust result here. Its
effect on the small response is modest on this machine because a request costs about 4 µs
of kernel time whichever way it is delivered: on loopback the server's `send` also runs
the client's receive path. On the 8 KB response, where the batching runtimes send a whole
batch at once, it is worth almost half the CPU.

### Two cores

The server gets two cores and starts one worker per core; `wrk` runs two threads on the
other two; 100 connections, three rounds.

| Server | `/json` | `/users` | `/compute` | Pipelined `/json` |
|---|---|---|---|---|
| Axum | 202.8k | 31.8k | 8.3k | 36k |
| Raw Tokio | 360.6k (354–361k) | 77.9k | 8.2k | 3.11M |
| Hand-written epoll | 305.5k | 83.1k | 8.4k | 2.66M |
| Glommio | 226.6k | 81.5k | 8.1k | 2.29M |
| Monoio (tuned) | 324.7k | 114.5k | 7.7k | 2.90M |
| **giong** | 304.7k (279–342k) | **118.4k** (104–122k) | 8.9k | 3.01M |
| **giong-http** | 350.4k (345–384k) | 107.9k | 8.9k | 3.06M |

**Not a ranking.** On four vCPUs the two `wrk` threads are as likely to be the limit as the
server: in an earlier two-core run giong's workers were only 73–94% busy. Raw Tokio and
`giong-http` lead on `/json`, giong and Monoio on `/users`, and the rest needs a bigger machine.

### One worker to N

`bench/workers.sh` measures how throughput grows with the number of workers: the server
gets the first N of its CPUs, the connection count grows with N (64 per worker), and the
report shows, besides requests per second, the CPU time per request and how busy the
server's and the load generator's CPUs were, so a run says which side set the number.
It can drive the server from a second machine over ssh (`LOAD_HOST`, `BENCH_HOST`), which
is how it should be run.

**On 14 cores.** An Intel i5-13500 desktop (6 performance cores and 8 efficiency cores,
20 threads, 64 GB) running Linux 6.18 under WSL2 on Windows 10. The server gets vCPUs 0–9,
`wrk` gets 10–19 with ten threads, and they talk over loopback; `/json`, three rounds of
6 s. Requests per second, median (the range for giong):

| Server | 1 worker | 2 | 4 | 6 | 8 | 10 |
|---|---|---|---|---|---|---|
| giong | 178k (165–181k) | 437k (414–462k) | 934k (818k–1.04M) | 1.24M (1.22–1.34M) | 1.50M (1.37–1.50M) ¹ | 1.33M ¹ |
| giong-http | 161k | 371k | 776k | 1.01M | 1.26M | 1.26M |
| Monoio (tuned) | 167k | 352k | 928k | 1.30M | 1.30M | 1.35M ¹ |
| Raw Tokio | 131k | 323k | 777k | 1.07M | 1.03M ² | 1.16M ² |
| Glommio | 172k | 322k | 768k | 1.06M | 984k | 1.13M |

CPU time per request, in µs:

| Server | 1 | 2 | 4 | 6 | 8 | 10 |
|---|---|---|---|---|---|---|
| giong | 5.58 | 4.58 | 4.29 | 4.79 | 5.26 | 6.66 |
| giong-http | 6.21 | 5.42 | 5.17 | 5.95 | 6.26 | 7.60 |
| Monoio (tuned) | 5.95 | 5.70 | 4.33 | 4.55 | 5.91 | 6.64 |
| Raw Tokio | 7.65 | 6.19 | 5.09 | 5.41 | 6.66 | 6.60 |
| Glommio | 5.80 | 6.24 | 5.24 | 5.68 | 8.11 | 8.75 |

¹ `wrk` was the limit (its CPUs 94–97% busy), so the number is a lower bound.
² Neither side was saturated (server 76–86% busy).

- **From 1 to 6 workers every server was the limit** (its CPUs 95–100% busy), so these
  rows measure the servers. giong had the highest median at 1 worker (Monoio's best round
  was higher) and was clearly ahead at 2, level with Monoio at 4 (the ranges overlap) and
  behind it at 6 (1.24M against 1.30M; Monoio's range was 876k–1.33M).
- **giong used the least CPU per request from 1 to 4 workers** (tied with Monoio at 4):
  4.3–5.6 µs, against 5.1–7.7 µs for raw Tokio.
- **The highest number of the run, 1.50M requests/s, was giong at 8 workers**, with `wrk`
  as the limit.
- **At 10 workers the machine is overloaded**: server and `wrk` keep 20 vCPUs busy on 14
  physical cores, everything flattens or drops, and p99 goes above 2 ms.

This run says nothing reliable about scaling efficiency. Going from one worker to two
gives 2.1 to 2.5 times the throughput for four of the five servers, which means the
one-worker numbers are too low, not that two workers do more than twice the work: WSL2
does not pin vCPUs to physical cores, and Windows 10 may run a lone busy vCPU on an
efficiency core. Some rows also vary by up to a third between rounds. The numbers needed
for that are from bare-metal Linux with the load generator on a second machine (see
[What has not been measured](#what-has-not-been-measured)). Recorded output, with the
machine details and how the run was checked: `bench/results/workers-i5-13500-wsl/`.

**On 4 vCPUs** (the virtual machine used for the other tables), two `wrk` threads on the
other two vCPUs could no longer keep up once the server had two workers, so that run only
shows the one-worker numbers (giong 218.9k, Monoio 214.2k, giong-http 186.1k, Glommio
175.8k, raw Tokio 151.1k). Recorded output: `bench/results/workers/`.

### HTTPS

`bench/tls.sh`: `giong-http` + `giong-tls` against Axum + `axum-server`, both on rustls with
`ring`, so the TLS library and the cryptography are the same and only the runtime and the
HTTP layer differ. The server gets one core and `wrk` three (the load generator has to
encrypt too); 50 connections, TLS 1.3 with AES-256-GCM, an ECDSA P-256 certificate, five
rounds. The server's core was 98–101% busy in every row, so the server was the limit.

| Server | `/json` | `/users` (8 KB) | New handshake per request |
|---|---|---|---|
| Axum, plain HTTP | 94.0k (86–97k) | 19.5k | |
| giong-http, plain HTTP | 150.3k (140–158k) | 55.3k (54–59k) | |
| Axum + rustls | 81.0k (80–83k) | 19.2k | 7.3k (6.9–7.6k) |
| **giong-http + giong-tls** | **144.8k (142–153k)** | **47.8k (43–49k)** | 7.1k (6.7–7.5k) |

- **On kept-alive connections giong's HTTPS is 1.8 times Axum + rustls on `/json` and 2.5
  times on `/users`.** Most of the `/users` factor is there without TLS (see above).
- **TLS costs giong 4% on `/json` and 14% on `/users`**, where there are more bytes to
  encrypt.
- **Handshakes are level** (7.1k against 7.3k per second): a handshake is almost all
  cryptography, and both use the same library for it.
- Axum is lower here than in the first table (94k against 161k) because `wrk` runs three
  threads: requests arrive less bunched and Tokio gets less work out of each `epoll_wait`.
  giong hardly depends on the shape of the load.
- One connection per request is not measured for plain HTTP: on loopback that test runs
  out of client ports (TIME_WAIT) at about 28,000 per second and measures the client.

**Kernel TLS pays off for large files, not for data in memory.** On bare metal (EPYC
4345P, Linux 7.0, the server on one core), giong-http serving a 1 MiB file with
`Body::file` and kernel TLS reaches 6,104 req/s against 4,484 with rustls in the process
(+36%, and 164 µs of CPU per request against 223): `sendfile` lets the kernel encrypt
straight from the page cache. nginx with kernel TLS and `sendfile` does 6,039 on the
same machine. With 64 KiB files, with bodies from memory and with small responses the
kernel is 4–22% slower than rustls, so `Ktls::Off` stays the default. A client receiving
64 KiB bodies gains 11% from letting the kernel decrypt. The whole table and a profile
are in the [`giong-tls` README](giong-tls/README.md#when-it-pays-off).

### PostgreSQL

`bench/postgres.sh`: [`giong-postgres`](giong-postgres/README.md#performance) against
tokio-postgres with deadpool-postgres, each client on one core, the same PostgreSQL 16
server on the other three, 64 tasks issuing statements back to back. Medians, statements
per second:

| Client, connections | One row by key | p99 | 100 rows | p99 |
|---|---|---|---|---|
| **giong-postgres, 3** | **240.7k** | **0.52 ms** | **54.8k** | **2.0 ms** |
| tokio-postgres + deadpool, 4 | 53.1k | 1.9 ms | 21.7k | 4.4 ms |
| tokio-postgres + deadpool, 64 (its best) | 93.8k | 1.5 ms | 28.6k | 4.1 ms |

Statements from every task share the pool's connections and are pipelined, so one client
core does 2.6 times the work of deadpool at its best. Over one shared connection both
libraries pipeline and the server's single backend is the limit; there the two are within
the spread of the rounds, giong-postgres 8–11% lower at the median. The crate's README has the full table, including what happens with more
connections than the server has cores.

### MySQL

`bench/mysql.sh`: [`giong-mysql`](giong-mysql/README.md#performance) against mysql_async and
SQLx on Tokio, each client on one core with its own pool, the same MySQL 8.0 server on the
other three, 64 tasks issuing statements back to back. Medians, statements per second:

| Client, connections | One row by key | p99 | 100 rows | p99 |
|---|---|---|---|---|
| **giong-mysql, 3** | **138.7k** | **0.91 ms** | **35.1k** | **3.2 ms** |
| SQLx, 64 (its best) | 29.3k | 3.9 ms | 10.4k | 9.1 ms |
| mysql_async, 4 (its best) | 19.0k | 5.5 ms | 8.9k | 10.6 ms |

Statements from every task share the pool's connections and are pipelined, so one client
core does 4.7 and 3.4 times the work of SQLx at its best, and the server becomes the limit.
One connection shared by every task does about what SQLx does with 64. The crate's README
has the full table, including mysql_async's default pool settings and more connections
than the server has cores.

### Redis

`bench/redis.sh`: [`giong-redis`](giong-redis/README.md#performance) against redis-rs (a
`MultiplexedConnection`) and fred on Tokio, all three pipelining the commands of 64 tasks,
each client on one core, the same Redis 7.0 server on the other three. Medians, commands
per second, and the client core's time per command:

| Client, connections | `GET` 64 B | µs/op | `SET` 64 B | µs/op | `MGET` of 10 | µs/op |
|---|---|---|---|---|---|---|
| **giong-redis, 1** | **618k** | **0.78** | **557k** | **0.77** | **219k** | **2.29** |
| redis-rs, 1 | 448k | 1.76 | 442k | 1.67 | 140k | 4.84 |
| fred, 1 | 314k | 2.42 | 287k | 2.58 | 110k | 6.91 |
| **giong-redis, 4** | **881k** | **0.98** | **779k** | **0.99** | **313k** | **2.30** |
| redis-rs, 4 | 475k | 2.04 | 491k | 1.97 | 198k | 4.96 |

1.3 to 1.9 times redis-rs, at half its CPU time per command or less. With 8 KiB values,
where about half of every client's core is time in the kernel, giong-redis does twice
redis-rs on one connection and the same on four. The crate's README has the full tables
and how many connections to use.

### Routing, JSON and middleware

`bench/web.sh`: [`giong-web`](giong-web/README.md#performance) against axum 0.8 with the
same serde handlers, each on one core, 50 connections. Medians of 5 rounds, requests per
second:

| Server | `/json` | `/users` (8 KB) | `/users/{id}` | With request ids, gzip, timeout: `/json` | `/users` |
|---|---|---|---|---|---|
| **giong-web** | **152.5k** | **28.0k** | **140.5k** | **126.2k** | **12.0k** |
| axum | 118.3k | 15.2k | 107.1k | 65.8k (tower-http) | 5.2k |

The router, parameters and serde cost giong-web about 5% against giong-http's bare handler
on `/json` in the same run. `/compute` is the same for both within noise; its p99 is higher
on giong-web (22.9 against 18.5 ms).

### File I/O

`bench/fs.sh`: 200,000 operations per measurement, five rounds, all data in the page cache,
so this measures what one operation costs, not the disk. giong and `std` use one thread.
Tokio hands every file operation to its blocking thread pool, so it is measured pinned to
one core and free to use all four.

| Operations per second | Whole 4 KiB file, 1 task | 64 tasks | 4 KiB at a random offset, 1 task | 64 tasks |
|---|---|---|---|---|
| `std` (blocking) | 556k | | 1.40M | |
| **giong** | **227k** (201–243k) | **292k** | **1.01M** (0.87–1.15M) | **1.26M** |
| Tokio, one core | 147k | 116k | 184k | 156k |
| Tokio, 4 cores | 29k | 81k | 33k | 74k |

- **Reads at an offset: 5.5 times Tokio with one task and 8 times with 64.** On Tokio every
  read is a hand-off to another thread and back; on giong it is one entry in a batch.
- **Whole small files (open, statx, read, close): 1.5 to 2.5 times Tokio.**
- **A blocking `std` call is faster for cached data** (28% for reads at an offset, 2.4
  times for whole files). What giong buys is a worker that never waits when the data is
  not cached, which this benchmark does not measure.

The HTTPS and file numbers were measured on 2026-11-11, on the same machine, with the
code as it was just before the busy-poll mode was merged in; the merge did not touch the
TLS, HTTP or file code. Everything above them was measured again on the merged code.

### What has not been measured

- **A physical network, and scaling on bare metal.** Everything here is loopback on one
  machine. The multi-core run is on a desktop under WSL2, where the load generator shares
  the machine and vCPUs are not pinned to cores. `bench/workers.sh` is ready for the real
  run: `BENCH_HOST=<server address> LOAD_HOST=<user@load machine> WORKERS="1 2 4 8 16"
  bench/workers.sh`.
- **Disks.** The file benchmark measures cached data only.
- **Newer Monoio and Glommio.** The comparison uses the latest crates.io releases
  (August and March 2024); their main branches may differ.

Reproduce with `bench/run.sh`, `bench/scale.sh`, `bench/workers.sh`, `bench/syscalls.sh`,
`bench/tls.sh`, `bench/fs.sh`, `bench/postgres.sh`, `bench/mysql.sh`, `bench/redis.sh`,
`bench/http-client.sh`, `bench/web.sh`, `bench/memory.sh` and `bench/compat.sh`; two cores with
`SERVER_CPU=2,3 LOAD_CPU=0,1 WRK_THREADS=2 CONNS=100 OUT=results/2core bench/run.sh 3`.
Recorded output is in `bench/results/`.

## Testing and safety

752 tests across the nine crates and the example API (`cargo test --workspace
--all-features`), run in release mode on all three io_uring setups and on the epoll
driver without a failure. The runtime's, HTTP's and TLS's main suites are below; the
database clients, giong-web, giong-mcp, giong-compat and the example API describe theirs
in their READMEs.

| Suite | What it checks |
|---|---|
| `tests/executor.rs` (14) | spawn and join, panics, abort, teardown, runtimes run back to back |
| `tests/time.rs` (8), `src/time.rs` (4) | sleep, timeout, ordering, 5,000 timers; the timing wheel against a reference model over 87 random operation sequences with timers from 1 ms to 30 days |
| `tests/net.rs` (20) | echo, 8 MB each way without a wrong byte, EOF, 500 and 1,300 connections, plain `std` clients; `send_file` of 9 MB and of ranges, past a file's end and to a closed peer; **a non-blocking socket under io_uring never reports `WouldBlock`** (it did on Linux 6.0 and 6.1) |
| `tests/unix.rs` (8) | echo over a socket file, 4 MB each way over `pair()`, bind and connect errors, 300 connections, `std` interop |
| `tests/udp.rs` (18) | IPv4 and IPv6, connected sockets, **a cancelled `recv_from` loses no datagram**, a burst of 64 datagrams complete and in order, truncation, 2,000 datagrams echoed through one buffer, a pool of 4 buffers not starved by one socket, three workers on one port, 1,000 sockets on port 0 each with a port of its own, sockets closed on drop |
| `tests/fs.rs` (20) | files intact up to 8 MB, positional reads and writes, open options like `std`, metadata, rename and remove, 200 tasks sharing one file, descriptors closed exactly once, an operation cancelled in flight, a file closed only after the blocking thread using it is done (epoll) |
| `tests/cancel.rs` (13) | **a buffer stays alive until the kernel confirms the cancellation**; 4 buffers for 32 connections; a stalled connection cannot starve another; a dropped `send_file` leaves nothing behind; teardown with everything in flight |
| `tests/multi.rs` (14) | several workers, cross-thread wake-ups, 30,000 channel messages, `SO_REUSEPORT`, SIGTERM, a panicking worker |
| `tests/shutdown.rs` (10) | **graceful shutdown**: remaining tasks finish within the grace period and are dropped when it expires; requests in progress are answered, idle connections close at once |
| `tests/spin.rs` (6) | the busy-poll mode behaves like the default: I/O, timers, cross-thread wake-ups, shutdown |
| `tests/driver.rs` (8), `tests/blocked.rs` (3) | choosing a driver in code or with `GIONG_DRIVER`; **with io_uring blocked by a seccomp filter, a runtime starts on epoll by itself**, and fails with GIONG-E0001 when io_uring was asked for; a task whose I/O never waits still lets the others run; a socket that gets the number of one whose connect failed is still woken |
| `tests/observe.rs` (8) | tasks named by the line that spawned them (`spawn`, `spawn_on`, `spawn_balanced`); **a pulse read from another thread names the task a blocked worker is stuck in**; counters and idle time; poll profiles by spawn location; sources reported until dropped |
| `giong-http` (136) | 22 client tests on real sockets (keep-alive, HTTPS, redirects, timeouts, truncated bodies, gzip, streamed bodies, proxies); 51 unit tests, among them 500,000 random inputs for the parser and the chunked decoder; 24 end-to-end tests on real sockets (pipelining, large bodies, file bodies, a request one byte at a time, 408/413/431, malformed requests, a panicking handler, graceful shutdown, a Unix socket, the client address, the connection limit); 14 end-to-end tests of request and response bodies streamed both ways; 12 end-to-end HTTPS tests with a rustls client; 13 documentation examples |
| `giong-tls` (64) | 14 client tests (system and built-in roots, client certificates, name and IP checks); TLS 1.2 and 1.3 with 4 MB each way, ALPN, a `recv` cancelled over and over, two tasks sending on one connection, a write cancelled half way (the client sees a cut-off stream, never a corrupt record), a file read and encrypted, broken or stalled handshakes; 27 kernel TLS tests (every cipher suite through the kernel in both roles, files sent with `sendfile`, key updates both ways, **the TLS 1.2 nonces read off the wire**, tampered and unexpected records, cancellation), run on Linux 5.15, 6.1, Ubuntu's 6.8 and 6.18 with the `tls` module |
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
  the corpus.
- **Soak** (`scripts/check.sh soak`, `tests/soak.rs`): TCP clients on every worker that
  close cleanly, abort, half-close or cancel receives, with UDP, file and cross-worker
  traffic alongside; afterwards no descriptor, memory or driver state may be left over.
  30 minutes on four workers: 39.5 million connections, resident memory flat at 15.5–15.7
  MB. CI runs it for two hours every night.
- **valgrind memcheck** over the single-threaded tests (`scripts/valgrind.sh`): no invalid
  access, no leak.
- **Load** on the example HTTP server: over 250,000 connections opened and closed, 5,000
  concurrent connections, the client killed abruptly six times. File descriptors always
  return to the idle count; memory settles and does not grow between runs.
- **Graceful shutdown under load**: SIGTERM during 40 slow requests. Without a grace
  period 156 of 200 requests were answered; with one, 400 of 400.
- **CI** runs formatting, clippy, the documentation build, a build with Rust 1.80, the
  repeated tests, the fallback setups and the epoll driver, the tests inside Docker,
  ThreadSanitizer, the loom models and the real 5.15, 6.0 and 6.1 kernels on every change,
  and fuzzing and the soak test every night.

Not verified, and you should know it:

- Neither sanitizer nor valgrind can see the kernel writing into memory, so cancellation
  safety rests on the design and on `tests/cancel.rs`.
- valgrind 3.22 cannot run the multi-threaded tests (it treats `io_uring_enter` as
  non-blocking, so threads starve each other); ThreadSanitizer covers those.
- `giong-tls` has had a [security review](docs/reviews/2026-11-giong-tls.md) of its own
  code and of every crate that uses it, which found and fixed two medium issues, but it was
  done by the project's AI agent, not by an independent auditor. The cryptography and the
  protocol are rustls's; giong's part is connecting rustls to the socket.

## Compared with Monoio and Glommio

| | giong 0.4 | Monoio 0.2.4 | Glommio 0.9.0 |
|---|---|---|---|
| Platforms | Linux: io_uring on ≥ 6.0, an epoll driver elsewhere | Linux ≥ 5.6, with an epoll/kqueue fallback (runs on macOS) | Linux ≥ 5.8, needs a raised memlock limit |
| Receiving | multishot, kernel-provided buffers | one submission per read, caller's buffer | direct `recv`, io_uring to wait |
| Accepting | multishot | one submission per accept | one submission per accept |
| Buffer API | ownership passing | ownership passing | borrowed (`AsyncRead` / `AsyncWrite`) |
| Timeout around a read | loses no data | needs the separate `cancelable_*` API | — |
| Ring defaults | `DEFER_TASKRUN` on | no flags; pass your own builder | — |
| Busy-poll option | yes | no | yes |
| Multi-core | built-in builder with graceful draining and signals | `#[monoio::main(threads = N)]` | executor pools placed by CPU topology |
| Scheduling | FIFO | FIFO | weighted queues with latency classes |
| I/O | TCP, UDP, Unix sockets, files | TCP, UDP, Unix sockets, files | TCP, UDP, Unix sockets, Direct I/O files |
| Ecosystem | its own HTTP/1.1 and HTTP/2 server and client, TLS for both sides, DNS, PostgreSQL, MySQL and Redis clients | TLS, HTTP, codecs, Tokio compatibility layer | channels, semaphores, rate control |
| Size | ~11,400 lines, plus ~14,800 for HTTP and TLS and ~17,500 for the database clients | ~25,000 lines | ~40,500 lines |

Monoio is the mature choice and runs in production at ByteDance; if you need
Tokio-compatible crates today, use it. Glommio is built for storage engines: Direct I/O and
priority scheduling are its strengths, not HTTP. giong's case is narrower: the receive
path (multishot with provided buffers, cancel safe without a second API), good defaults,
the latency dial, a strict HTTP server on top, and code small enough to audit.

## Limitations

- **Name resolution reads files and DNS only.** `connect` and `lookup_host` resolve names
  without blocking, from `/etc/hosts` and the DNS servers in `/etc/resolv.conf`; other
  Name Service Switch sources (mDNS, LDAP) are not used. `bind` still resolves with a
  blocking call, which only matters at startup. `fs::read_dir` reads each batch of
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
- **Unused so far:** fixed files, zero-copy send, `SQPOLL`.

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

```
src/
  lib.rs        entry point and docs
  runtime.rs    Builder, Runtime, Handle, the event loop, shutdown, signals
  executor.rs   tasks, wakers, spawn, JoinHandle
  driver/       the drivers behind one interface (mod.rs: operations, choosing, falling back)
    uring.rs    io_uring: submit and reap, operation tracking, cancellation, backpressure
    epoll.rs    epoll: readiness, edge-triggered registration, the per-task budget
    blocking.rs the thread pool for file operations on epoll
  op.rs         a single operation as a future
  buf.rs        IoBuf / IoBufMut, the kernel-shared buffer pool, RecvBuf
  net.rs        the core shared by stream sockets, TcpListener, TcpStream
  net/unix.rs   UnixListener, UnixStream
  net/udp.rs    UdpSocket, Datagram
  fs.rs         File, OpenOptions, Metadata, the path operations
  time.rs       timing wheel, sleep, timeout
  sync.rs       mpsc channel
  observe.rs    pulses, snapshots, profiles and sources, for diagnostics and giong-mcp
giong-http/     the HTTP/1.1 crate: the server (parser, chunked decoding, connections, HTTPS behind `tls`), the client behind `client`
giong-tls/      the TLS crate on rustls (TlsAcceptor, TlsConnector, TlsStream)
giong-postgres/ the PostgreSQL client crate (wire protocol, authentication, statement cache, Pool)
giong-mysql/    the MySQL client crate (client/server protocol, authentication, statement cache, Pool)
giong-redis/    the Redis client crate (RESP3, pipelining across tasks, Pub/Sub, Pool)
giong-web/      routing, middleware and JSON on giong-http (Router, App, Req, Error, middleware/)
giong-mcp/      the MCP server for agents: Streamable HTTP, tools over giong::observe
giong-compat/   Compat: the Tokio and futures-io I/O traits for giong's streams
examples/       echo.rs, http.rs, udp_echo.rs, cat.rs; rest-api/, a complete REST API (PostgreSQL, a Redis cache, HTTPS)
tests/          executor, time, net, unix, udp, fs, cancel, multi, shutdown, spin, driver, blocked, ...
bench/          run.sh, scale.sh, workers.sh, syscalls.sh, tls.sh, fs.sh, postgres.sh, mysql.sh, redis.sh, http-client.sh, web.sh, rest-api.sh, compat.sh, the programs compared against, results/
scripts/        check.sh (repeated tests, fallback setups, epoll, seccomp, ThreadSanitizer), kernels.sh, no-io-uring.py, valgrind.sh
assets/         logo, icons, social preview (sources in assets/README.md)
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you state otherwise, any contribution you submit is licensed the
same way, without additional terms.
