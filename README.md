<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-dark.svg">
    <img alt="giong" src="https://raw.githubusercontent.com/giong-run/Giong/main/assets/giong-logo-light.svg" width="340">
  </picture>
</p>

<p align="center"><b>A thread-per-core async runtime for Linux, built directly on io_uring.</b></p>

<p align="center">
  <a href="https://github.com/giong-run/Giong/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/giong-run/Giong/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue">
  <img alt="Platform: Linux 6.0+" src="https://img.shields.io/badge/platform-Linux%206.0%2B-informational">
</p>

Named after Thánh Gióng, the child of Vietnamese legend who lay silent for three years and
then rose a giant: giong costs next to nothing while idle and uses every core the moment
load arrives.

> **Status: 0.2.** Complete enough to write real servers: TCP, UDP, Unix sockets, files,
> timers, multi-core, graceful shutdown, and two companion crates,
> [`giong-http`](giong-http/README.md) (a strict HTTP/1.1 server) and
> [`giong-tls`](giong-tls/README.md) (TLS 1.2/1.3 on rustls), for serving HTTPS directly.
> 169 tests pass on three io_uring setups and on real 6.0 and 6.1 kernels; ThreadSanitizer
> and valgrind are clean, loom model-checks the cross-thread wake-ups, the HTTP parser is
> fuzzed and a soak test churns connections for two hours every night. It has **not** run
> in production, and there is no HTTP/2 yet. See [Limitations](#limitations).

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
- **Small enough to read.** About 5,400 lines for the runtime, three dependencies, and a
  comment on every `unsafe` block.

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

- **Linux 6.0 or newer** (6.1+ recommended). Developed and tested on 6.18.
- Rust 1.80 or newer.
- giong does **not** build on Windows or macOS: io_uring is a Linux interface. On Windows
  use WSL2 with a 6.x kernel (`uname -r`; run `wsl --update` if it is older), a Linux
  virtual machine, or Docker started with `--security-opt seccomp=unconfined` (Docker's
  default profile blocks io_uring).

## Quick start

```bash
git clone https://github.com/giong-run/Giong.git && cd Giong
cargo test --workspace --all-features   # the whole suite, all three crates
cargo run --release --example echo      # TCP echo server on port 7000
cargo run --release --example http      # hand-written HTTP/1.1 server on port 3006
cargo run --release --example udp_echo  # UDP echo server on port 7001
cargo run --release --example cat -- Cargo.toml   # read a file through the ring
```

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
| Configuration | `.workers(n)`, `.pin_cores(bool)`, `.spin_before_park(duration)`, `.ring_entries(n)`, `.recv_buffers(count, size)`, `.shutdown_on_signals(bool)`, `.shutdown_grace(duration)` |
| Tasks | `spawn(fut) -> JoinHandle<T>`, `handle.abort()`, `yield_now()`; futures need not be `Send` |
| TCP | `TcpListener::bind` / `accept`; `TcpStream::connect` / `recv` / `read` / `write` / `write_all` / `shutdown` / `set_nodelay` |
| Unix sockets | `UnixListener::bind` / `accept`; `UnixStream::connect` / `pair`, with the same read and write methods as `TcpStream` |
| UDP | `UdpSocket::bind` / `connect`; `recv_from` / `recv` (multishot, pool buffer); `read_from` / `read` (your buffer); `send_to` / `send`; `set_broadcast` |
| Files | `fs::File::open` / `create` / `options`; `read_at` / `write_at` / `write_all_at` / `sync_all` / `sync_data` / `metadata` / `len` / `close`; `fs::read`, `write`, `metadata`, `rename`, `remove_file`, `create_dir`, `remove_dir` |
| Time | `time::sleep`, `sleep_until`, `timeout(duration, fut)` |
| Shutdown | `shutdown_signal()`, `until_shutdown(fut)`, `is_shutting_down()`, `Handle::shutdown()` |
| Across threads | `Handle::spawn_on(worker, \|\| async { .. })`, `sync::mpsc::channel()` |
| HTTP | [`giong-http`](giong-http/README.md): `Server::serve` / `serve_unix` / `serve_tls`, `Request`, `Response` |
| TLS | [`giong-tls`](giong-tls/README.md): `TlsAcceptor`, `TlsStream` |

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
| **Accept and receive** | Multishot accept; multishot receive with a provided buffer ring, for TCP, Unix sockets and UDP (`recvmsg`). | One submission for the life of a listener, connection or socket. The kernel picks a buffer only when data arrives, so the buffer is warm in cache and idle connections hold none. |
| **Writes and files** | Ownership passing: `write_all(buf)` returns `(result, buf)`. File operations are ordinary entries in the same ring. | The kernel keeps a pointer to the buffer until it is done; this API makes that sound in Rust. Files need no pool of blocking threads. |
| **Cancellation** | When a future is dropped mid-flight, its buffer moves to the driver, a cancel is sent, and the memory is freed only when the kernel confirms. | Avoids use-after-free, the classic bug of completion-based runtimes. |
| **Backpressure** | A slow reader has its multishot receive paused (TCP then slows the sender) and its backlog copied off the pool. A dry pool reclaims buffers that hold unread data. An accept backlog over 1,024 pauses the accept. | One stalled connection cannot starve the rest. |
| **Timers** | A hierarchical timing wheel in user space (six levels of 64 slots, 1 ms ticks). | Arming and cancelling are O(1) and make no system call. Nearly every timer is an I/O timeout that is cancelled before it fires. |
| **Latency dial** | Optionally busy-poll the ring for a while before sleeping. | A sleeping thread must be woken by an interrupt and rescheduled, which costs tens of microseconds, more in a virtual machine. |
| **Panics** | A panic in a task is caught and reported through its `JoinHandle`. | One bad connection does not take down the worker. |
| **Shutdown** | With a grace period, a worker keeps running its tasks after its main future returns, until they finish or the period ends. Then: cancel everything, wait for the kernel to confirm, free. If confirmation does not come, leak on purpose. | Requests in progress are answered. Memory safety matters more than a few megabytes at exit. |
| **Old kernels** | Three ring setups, tried in order: `DEFER_TASKRUN` (6.1+), `COOP_TASKRUN` (6.0), no flags. | The same code runs on 6.0; the two fallbacks are tested on every change by forcing them. |

## Benchmarks

Every server implements the same three routes (`bench/`): `/json` (a 27-byte body),
`/users` (8 KB of JSON built per request) and `/compute` (a recursive Fibonacci).
`giong`, Monoio, raw Tokio and the hand-written epoll server are deliberately the same
program, with the same minimal HTTP parser, so they compare runtimes; `giong-http` is the
library, with strict parsing, a `Date` header, timeouts and limits. Each server is pinned
to its own core and `wrk` runs on another. Servers are measured in interleaved rounds and
the tables show the median, with the range in parentheses. Machine: a 4-vCPU virtual
machine (Xeon 2.1 GHz), Linux 6.18, Rust 1.97, Node 22, Bun 1.4, Tokio 1.54, Monoio 0.2.4,
Glommio 0.9.0, wrk 4.1.

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
is how it should be run. On this 4-vCPU machine (`/json`, three rounds of 6 s):

| Server | 1 worker | 2 workers | CPU per request, 1 → 2 | Limited at 2 by |
|---|---|---|---|---|
| giong | 218.9k | 311.1k | 4.50 → 5.09 µs | `wrk` (99% busy; server 81%) |
| giong-http | 186.1k | 342.8k | 5.31 → 5.32 µs | `wrk` (97%; server 93%) |
| Monoio (tuned) | 214.2k | 337.6k | 4.59 → 4.75 µs | `wrk` (99%; server 82%) |
| Raw Tokio | 151.1k | 351.9k | 6.51 → 5.15 µs | `wrk` (98%; server 93%) |
| Glommio | 175.8k | 226.9k | 5.63 → 8.69 µs | server (99%) |

With one worker every server is the bottleneck; with two, two `wrk` threads on the other
two vCPUs can no longer keep up, so the two-worker numbers are lower bounds and say
nothing about scaling. A worker that is not saturated parks and wakes more often, which is
why giong's CPU per request rises at 81% busy. Real scaling numbers need a machine with
more cores and a separate load generator (see [What has not been measured](#what-has-not-been-measured)).
Recorded output: `bench/results/workers/`.

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

- **More than two cores, and a physical network.** Everything here is loopback inside a
  4-vCPU virtual machine, where the load generator competes for the same few cores.
  `bench/workers.sh` is ready for the real run: `BENCH_HOST=<server address>
  LOAD_HOST=<user@load machine> WORKERS="1 2 4 8 16" bench/workers.sh`.
- **Disks.** The file benchmark measures cached data only.
- **Newer Monoio and Glommio.** The comparison uses the latest crates.io releases
  (August and March 2024); their main branches may differ.

Reproduce with `bench/run.sh`, `bench/scale.sh`, `bench/workers.sh`, `bench/syscalls.sh`,
`bench/tls.sh` and `bench/fs.sh`; two cores with
`SERVER_CPU=2,3 LOAD_CPU=0,1 WRK_THREADS=2 CONNS=100 OUT=results/2core bench/run.sh 3`.
Recorded output is in `bench/results/`.

## Testing and safety

169 tests (`cargo test --workspace --all-features`), repeated in release mode on all three
io_uring setups without a failure:

| Suite | What it checks |
|---|---|
| `tests/executor.rs` (14) | spawn and join, panics, abort, teardown, runtimes run back to back |
| `tests/time.rs` (7), `src/time.rs` (4) | sleep, timeout, ordering, 5,000 timers; the timing wheel against a reference model over 87 random operation sequences with timers from 1 ms to 30 days |
| `tests/net.rs` (13) | echo, 8 MB each way without a wrong byte, EOF, 500 and 1,300 connections, plain `std` clients |
| `tests/unix.rs` (7) | echo over a socket file, 4 MB each way over `pair()`, bind and connect errors, 300 connections, `std` interop |
| `tests/udp.rs` (16) | IPv4 and IPv6, connected sockets, **a cancelled `recv_from` loses no datagram**, a burst of 64 datagrams complete and in order, truncation, 2,000 datagrams echoed through one buffer, a pool of 4 buffers not starved by one socket, three workers on one port, sockets closed on drop |
| `tests/fs.rs` (12) | files intact up to 8 MB, positional reads and writes, open options like `std`, metadata, rename and remove, 200 tasks sharing one file, descriptors closed exactly once, an operation cancelled in flight |
| `tests/cancel.rs` (10) | **a buffer stays alive until the kernel confirms the cancellation**; 4 buffers for 32 connections; a stalled connection cannot starve another; teardown with everything in flight |
| `tests/multi.rs` (10) | several workers, cross-thread wake-ups, 30,000 channel messages, `SO_REUSEPORT`, SIGTERM, a panicking worker |
| `tests/shutdown.rs` (9) | **graceful shutdown**: remaining tasks finish within the grace period and are dropped when it expires; requests in progress are answered, idle connections close at once |
| `tests/spin.rs` (6) | the busy-poll mode behaves like the default: I/O, timers, cross-thread wake-ups, shutdown |
| `giong-http` (46) | 15 unit tests, among them 500,000 random inputs for the parser and the chunked decoder; 17 end-to-end tests on real sockets (pipelining, large bodies, a request one byte at a time, 408/413/431, malformed requests, a panicking handler, graceful shutdown, a Unix socket); 9 end-to-end HTTPS tests with a rustls client; 5 documentation examples |
| `giong-tls` (8) | TLS 1.2 and 1.3 with 4 MB each way, ALPN, a `recv` cancelled over and over, two tasks sending on one connection, a write cancelled half way (the client sees a cut-off stream, never a corrupt record), broken or stalled handshakes |
| Other (7) | slab, the io_uring setup tiers, 5 documentation examples |

Also:

- **ThreadSanitizer** (`scripts/check.sh tsan`, Rust nightly) over 157 tests of all three
  crates: no data race. Two things are handled specially, both because of the tool:
  sanitizer builds close descriptors with `close(2)` (it cannot see a close that goes
  through the ring and then reports races on reused descriptor numbers), and the SIGTERM
  test is skipped (the sanitizer delivers signals only inside libc calls it intercepts,
  and giong's threads wait in raw system calls).
- **The fallback io_uring setups** (`scripts/check.sh fallback`): the whole suite again
  with `GIONG_RING_TIER=coop` (the flags of kernel 6.0) and `basic` (no flags at all).
- **Real 6.0 and 6.1 kernels** (`scripts/kernels.sh`): the test binaries booted in QEMU
  on Linux 6.0.19 and 6.1.155, in CI on every change. It found that a runtime could fail
  to start on 6.0, which pins one page more than the buffer ring needs; fixed.
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
  repeated tests, the fallback setups, ThreadSanitizer, the loom models and the real 6.0
  and 6.1 kernels on every change, and fuzzing and the soak test every night.

Not verified, and you should know it:

- Neither sanitizer nor valgrind can see the kernel writing into memory, so cancellation
  safety rests on the design and on `tests/cancel.rs`.
- valgrind 3.22 cannot run the multi-threaded tests (it treats `io_uring_enter` as
  non-blocking, so threads starve each other); ThreadSanitizer covers those.
- `giong-tls` has not had an independent security audit. The cryptography and the protocol
  are rustls's; giong's part is connecting rustls to the socket.

## Compared with Monoio and Glommio

| | giong 0.2 | Monoio 0.2.4 | Glommio 0.9.0 |
|---|---|---|---|
| Platforms | Linux ≥ 6.0 | Linux ≥ 5.6, with an epoll/kqueue fallback (runs on macOS) | Linux ≥ 5.8, needs a raised memlock limit |
| Receiving | multishot, kernel-provided buffers | one submission per read, caller's buffer | direct `recv`, io_uring to wait |
| Accepting | multishot | one submission per accept | one submission per accept |
| Buffer API | ownership passing | ownership passing | borrowed (`AsyncRead` / `AsyncWrite`) |
| Timeout around a read | loses no data | needs the separate `cancelable_*` API | — |
| Ring defaults | `DEFER_TASKRUN` on | no flags; pass your own builder | — |
| Busy-poll option | yes | no | yes |
| Multi-core | built-in builder with graceful draining and signals | `#[monoio::main(threads = N)]` | executor pools placed by CPU topology |
| Scheduling | FIFO | FIFO | weighted queues with latency classes |
| I/O | TCP, UDP, Unix sockets, files | TCP, UDP, Unix sockets, files | TCP, UDP, Unix sockets, Direct I/O files |
| Ecosystem | its own HTTP/1.1 server and server-side TLS; nothing else | TLS, HTTP, codecs, Tokio compatibility layer | channels, semaphores, rate control |
| Size | ~5,400 lines, plus ~2,250 for HTTP and TLS | ~25,000 lines | ~40,500 lines |

Monoio is the mature choice and runs in production at ByteDance; if you need an HTTP
client, HTTP/2 or Tokio-compatible crates today, use it. Glommio is built for storage
engines: Direct I/O and priority scheduling are its strengths, not HTTP. giong's case is
narrower: the receive path (multishot with provided buffers, cancel safe without a second
API), good defaults, the latency dial, a strict HTTP server on top, and code small enough
to audit.

## Limitations

- **No asynchronous DNS.** `bind` and `connect` resolve names with a blocking call. Also
  missing: Unix datagram sockets, multicast, reading directories, `set_len`, file locks,
  `O_DIRECT` with registered buffers.
- **No HTTP/2, and HTTP bodies are not streamed.** `giong-http` receives a body in full
  before it calls the handler; one over the limit is refused.
- **TLS is server side only.** No TLS client, no kTLS, no independent audit of `giong-tls`.
- **Not compatible with the Tokio ecosystem.** Crates that need the Tokio runtime (hyper,
  sqlx, reqwest) do not run on giong. Crates that only use `std::future` do.
- **Tasks do not migrate.** An overloaded core is not helped by an idle one; spread work
  yourself with `spawn_on` or a channel.
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
- [CHANGELOG.md](CHANGELOG.md) — what changed in each release.

```
src/
  lib.rs        entry point and docs
  runtime.rs    Builder, Runtime, Handle, the event loop, shutdown, signals
  executor.rs   tasks, wakers, spawn, JoinHandle
  driver.rs     io_uring: submit and reap, operation tracking, cancellation, backpressure
  op.rs         a single operation (one SQE, one CQE) as a future
  buf.rs        IoBuf / IoBufMut, the kernel-shared buffer pool, RecvBuf
  net.rs        the core shared by stream sockets, TcpListener, TcpStream
  net/unix.rs   UnixListener, UnixStream
  net/udp.rs    UdpSocket, Datagram
  fs.rs         File, OpenOptions, Metadata, the path operations
  time.rs       timing wheel, sleep, timeout
  sync.rs       mpsc channel
giong-http/     the HTTP/1.1 server crate (parser, chunked decoding, connections, HTTPS behind the `tls` feature)
giong-tls/      the server-side TLS crate on rustls (TlsAcceptor, TlsStream)
examples/       echo.rs, http.rs, udp_echo.rs, cat.rs
tests/          executor, time, net, unix, udp, fs, cancel, multi, shutdown, spin
bench/          run.sh, scale.sh, syscalls.sh, tls.sh, fs.sh, the servers compared against, results/
scripts/        check.sh (repeated tests, fallback setups, ThreadSanitizer), valgrind.sh
assets/         logo, icons, social preview (sources in assets/README.md)
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you state otherwise, any contribution you submit is licensed the
same way, without additional terms.
