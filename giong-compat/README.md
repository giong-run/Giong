# giong-compat

The I/O traits of Tokio and of the futures crates for [giong](../README.md)'s streams, so that protocol crates which are generic over their transport run on giong: TLS (tokio-rustls, futures-rustls), WebSocket (tokio-tungstenite), HTTP/2 (h2), hyper's connection API, framing (tokio-util's codecs) and the like. No Tokio runtime is involved: giong drives everything, on its own threads.

```toml
[dependencies]
giong = "0.4"
giong-compat = "0.4"
```

```rust
use giong::net::TcpStream;
use giong_compat::Compat;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn main() -> std::io::Result<()> {
    giong::block_on(async {
        let mut stream = Compat::new(TcpStream::connect("127.0.0.1:7000").await?);
        stream.write_all(b"ping\n").await?;
        stream.flush().await?; // writes are buffered until a flush, or until a read waits
        let mut reply = [0; 5];
        stream.read_exact(&mut reply).await?;
        Ok(())
    })
}
```

`Compat<S>` wraps a `giong::net::TcpStream`, a `giong::net::UnixStream` or, with the `tls` feature, a `giong_tls::TlsStream`, and implements `AsyncRead`, `AsyncWrite` and `AsyncBufRead` from `tokio::io` (feature `tokio`) and from `futures_io` (feature `futures-io`). Both features are on by default and pull in the trait definitions only: `tokio` with its default features off, so no runtime, reactor or timer. The libraries build on Rust 1.80.

## When to use it, and when not

Use the native crate when there is one. It is built on giong's owned buffers and batched system calls, and is faster than the same protocol through this adapter.

| You need | Use |
|---|---|
| An HTTP/1.1 or HTTP/2 server or client | [`giong-http`](../giong-http/README.md); [`giong-web`](../giong-web/README.md) for routing and JSON |
| TLS | [`giong-tls`](../giong-tls/README.md) |
| PostgreSQL, MySQL, Redis | [`giong-postgres`](../giong-postgres/README.md), [`giong-mysql`](../giong-mysql/README.md), [`giong-redis`](../giong-redis/README.md) |
| h2's own API (HTTP/2 frames and streams, or a library built on h2) | [h2](https://docs.rs/h2) over `Compat`, plain or over `giong-tls` with ALPN `h2` |
| WebSocket | [tokio-tungstenite](https://docs.rs/tokio-tungstenite) (`accept_async`, `client_async`) over `Compat` |
| A protocol framed by length or by line | [tokio-util](https://docs.rs/tokio-util)'s `Framed` with a codec over `Compat` |
| hyper's API in particular | `hyper::{client, server}::conn` over `hyper_util::rt::TokioIo::new(Compat::new(..))`; a server needs a timer, [see below](#hyper-servers-need-a-timer) |
| A crate that opens its own connections with Tokio (reqwest, tonic's `transport`, sqlx, most SDKs) | Not `Compat`: see below |

### What works

A crate works through `Compat` when **it takes the stream from you and you drive it**: a function such as `tokio_rustls::TlsConnector::connect(name, stream)`, `tokio_tungstenite::client_async(request, stream)`, `h2::client::handshake(stream)` or `hyper::client::conn::http1::handshake(io)`, which returns a future, or a connection object, that your task (or a task you start with `giong::spawn`) polls. Futures that are not `Send` are fine, since giong's tasks stay on their worker.

These are tested in [`tests/`](tests), on giong's io_uring driver, its older ring setups and its epoll driver, with no Tokio runtime in the process:

| Crate | What the tests do |
|---|---|
| tokio-rustls 0.26 | A tokio-rustls client against a giong-tls server, and a giong-tls client against a tokio-rustls server (ALPN, `close_notify`), up to 1 MiB per message |
| futures-rustls 0.26 | A futures-rustls client (futures-io traits) against a giong-tls server |
| tokio-tungstenite 0.29 | A WebSocket echo server and client: text, a 4 MiB binary message, 1,000 messages queued before any is read, ping and pong, the close handshake |
| h2 0.4 | An HTTP/2 server and client: 100 streams at once on one connection with bodies over several flow-control windows; and h2 over giong-tls with ALPN `h2` |
| hyper 1 (HTTP/1) | hyper's client against a giong-http server, and giong-http's client against a hyper server, with keep-alive and bodies up to 500 KB; a hyper server with a timer on giong's timers closes a connection whose request head does not arrive in time |
| hyper 1 (HTTP/2) | hyper's client and server over `Compat`, with giong's tasks as hyper's executor: 100 requests at once on one connection, bodies up to 300 KB |
| tokio-util 0.7 | `LengthDelimitedCodec` over a Unix socket (200 frames up to 1 MB) and `LinesCodec` over TCP |

### What does not

`Compat` provides I/O traits, nothing else. A crate does not work through it when it

- **opens its own connections with `tokio::net`** (reqwest, tonic's `transport`, sqlx, redis-rs, most cloud SDKs): it panics with *"there is no reactor running, must be called from the context of a Tokio 1.x runtime"*, or never finishes;
- **uses Tokio's timers or spawns** (`tokio::time::sleep`, `tokio::time::timeout`, `tokio::spawn`): the same panic. hyper takes its timer and executor from you instead: see [hyper servers need a timer](#hyper-servers-need-a-timer);
- **requires `Send`** (bounds such as `T: Send` or `F: Future + Send + 'static`): `Compat` is not `Send`, because the stream belongs to one worker. The compiler says so.

Some of these crates take a custom connector or executor, and can work with one built on giong, but each has to be tried. Otherwise, run the crate on a Tokio runtime in a thread of its own.

### hyper servers need a timer

hyper's server enforces no timeout of its own: `header_read_timeout` needs a `hyper::rt::Timer`, and without one a client that sends half a request head and then nothing holds the connection, and the task serving it, for as long as it likes. Setting the timeout without a timer panics. giong's timers can back one:

```rust
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// hyper's timer, on giong's timers.
#[derive(Clone, Copy, Debug)]
struct GiongTimer;

struct GiongSleep(giong::time::Sleep);

impl std::future::Future for GiongSleep {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        Pin::new(&mut self.0).poll(cx)
    }
}

impl hyper::rt::Sleep for GiongSleep {}

impl hyper::rt::Timer for GiongTimer {
    fn sleep(&self, duration: Duration) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(GiongSleep(giong::time::sleep(duration)))
    }
    fn sleep_until(&self, deadline: Instant) -> Pin<Box<dyn hyper::rt::Sleep>> {
        Box::pin(GiongSleep(giong::time::sleep_until(deadline)))
    }
}

// For each accepted connection:
// hyper::server::conn::http1::Builder::new()
//     .timer(GiongTimer)
//     .header_read_timeout(Duration::from_secs(10))
//     .serve_connection(hyper_util::rt::TokioIo::new(Compat::new(tcp)), service)
```

[`tests/hyper.rs`](tests/hyper.rs) checks that a server built this way closes such a connection when the timeout ends.

hyper's HTTP/2, client and server, also takes an executor for the tasks it starts. They need not be `Send`, so giong's tasks do:

```rust
#[derive(Clone, Copy, Debug)]
struct GiongExec;

impl<F: std::future::Future + 'static> hyper::rt::Executor<F> for GiongExec {
    fn execute(&self, future: F) {
        giong::spawn(future);
    }
}

// hyper::server::conn::http2::Builder::new(GiongExec).timer(GiongTimer) ...
// hyper::client::conn::http2::handshake(GiongExec, io) ...
```

## How it behaves

- **Writes are buffered**, as with `BufWriter` or a TLS stream. `poll_write` copies the bytes and returns; they are sent on `flush`, when the buffer is full, or when a read on the same task is about to wait. Call `flush` (or `shutdown`) where the data must have gone out; every crate written for these traits does. A writer in another task than the reader must flush: a read only sends what its own task wrote. While one buffer is being sent the next one fills, so a long transfer keeps the socket busy.
- **The write buffer is bounded**: `Compat::new(stream).write_buffer(limit)`, 64 KiB by default, from 1 byte to 1 GiB (`GIONG-E1403` otherwise). A busy stream holds up to twice the limit; an idle one holds no buffer.
- **`TCP_NODELAY` is turned on** for TCP streams (plain or under TLS), as every giong crate does. `Compat` already gathers small writes until a flush, and Nagle's algorithm on top of that only delays them: the HTTP/2 test with 100 streams took 2.3 s with it and 21 ms without. `compat.get_ref().set_nodelay(false)` undoes it.
- **A failed send is reported by the next write, flush or shutdown**, with the kernel's own error; after that, writes, flushes and shutdowns fail with `GIONG-E1401`. So does a failed shutdown (a TLS stream that cannot send its `close_notify`). Writing after `shutdown` (or `close`) has started fails with `GIONG-E1402`. Reading still works in all these cases. A shutdown that finds the connection already reset counts as done, as with Tokio's `TcpStream`.
- **Dropping a `Compat` discards what was written but not flushed**, like dropping a `BufWriter`. Sends in flight are cancelled the way giong cancels any operation: their buffers stay alive until the kernel lets go.
- **One task reads and one task writes at a time.** `tokio::io::split` and `futures::io::AsyncReadExt::split` work, and so does splitting a `Framed` or a WebSocket stream.
- **Cancel safe.** A read that is dropped loses no data. A write either copies bytes and says how many, or copies nothing. [`tests/cancel.rs`](tests/cancel.rs) drops reads, writes, flushes and whole streams at random (seeded, so a failure repeats), over TCP and giong-tls and through TLS 1.3 key updates, and checks every byte and that nothing leaks.
- **`fill_buf` holds a buffer of the worker's pool** until `consume` takes all of it, or a read or `consume` of part of it moves the rest to the heap. Do not wait on something else between `fill_buf` and `consume`: with many connections doing that, the worker's receive pool runs dry and every socket on the worker stalls. `consume` of more than `fill_buf` returned is ignored, as with std's and Tokio's `BufReader`.
- **Reading after the end of the stream** keeps returning 0 bytes. `into_inner().await` flushes and hands back the stream with the bytes received but not yet read, to go on with giong's own API.

## What it costs

giong reads into buffers from the worker's pool and writes buffers it owns until the kernel is done with them; these traits read into, and write from, memory the caller lends for one call. So `Compat` copies:

- **Reading:** each byte once more than giong's own `recv`, from the pool buffer into the caller's buffer. Through `AsyncBufRead` (`fill_buf`, then `consume` of the whole chunk) there is no extra copy. When a caller takes only part of a chunk, the rest moves to a heap buffer at once, so that a slow reader never holds on to the worker's pool.
- **Writing:** each byte once, into the write buffer, which giong's `write_all` then sends. One small allocation per send (the future that sends it).

Measured with [`bench/compat.sh`](../bench/compat.sh): one TCP stream over loopback, the measured side on one pinned core and the other end on another, the same peer for every variant, 9 runs interleaved. Median, with the range of the 9 runs in brackets. The machine is a 4-vCPU cloud VM (Xeon at 2.1 GHz, Linux 6.18), where bulk transfers over loopback vary from run to run and from one session to the next: read the medians, the spread, and the four sessions below. This session ran on `main` at `8632eb7`.

| Workload | giong's own API | `Compat` | `Compat`, `AsyncBufRead` | Tokio |
|---|---|---|---|---|
| Receive 2 GiB (peer writes 256 KiB at a time), MiB/s | 3025 [2716–3134] | 2943 [2628–3550] | 3042 [2881–3247] | 2932 [2788–3519] |
| CPU per KiB received | 159 ns | 180 ns | 152 ns | 131 ns |
| Send 2 GiB from a 64 KiB buffer, MiB/s | 2475 [2042–2728] | 2220 [1918–2442] | | 2389 [1915–3312] |
| CPU per KiB sent | 382 ns | 437 ns | | 408 ns |
| 64-byte request and reply, round trips/s | 29,939 [26,924–32,057] | 29,368 [27,930–32,680] | | 31,572 [29,479–33,692] |
| CPU per round trip | 15.7 µs | 15.9 µs | | 14.9 µs |

CPU is the measured thread's own (user and system, `CLOCK_THREAD_CPUTIME_ID`).

The cost of `Compat` moves from one session of 9 runs to the next by more than the difference it measures. Four sessions on the same machine type, each the medians of 9 runs, `Compat` against giong's own API:

| Session (commit) | Send, MiB/s | Send, CPU per KiB | Receive with `read`, CPU per KiB | Receive with `AsyncBufRead`, CPU per KiB | Request and reply, round trips/s | CPU per round trip |
|---|---|---|---|---|---|---|
| 1 (`f992163`) | −12.5% | +13.8% | +5.8% | −2.2% | −2.8% | +4.7% |
| 2 (`0bd788a`) | −16.1% | +19.4% | +11.8% | +3.9% | +1.2% | +3.4% |
| 3 (`0bd788a`) | −20.1% | +25.1% | +13.2% | +8.7% | −0.7% | +3.5% |
| 4 (`8632eb7`, the table above) | −10.3% | +14.6% | +13.1% | −4.6% | −1.9% | +1.4% |

What the numbers say:

- **Sending** through `Compat` gives up 10–20% of throughput and spends 14–25% more CPU per byte than passing an owned buffer to giong's `write_all`. That is the copy: under callgrind, `memcpy` is 96% of the user-space instructions `Compat` runs on this workload. Tokio, which writes straight from the caller's memory, is within 6% of giong's throughput in every session.
- **Receiving** through `read` costs 6–13% more CPU per byte than giong's own `recv`, for the copy into the caller's buffer, at the same throughput. Through `AsyncBufRead` (`fill_buf`, then `consume`) there is no copy: the four sessions put it between 5% below and 9% above giong's own cost, which is the noise between sessions.
- **Request and reply** differ by less than the noise in throughput; the CPU per round trip is 1–5% higher. Callgrind counts about 1,000 more user-space instructions per round trip (the boxed send, the flush, one more poll), against roughly 16 µs spent mostly in the kernel.
- **Tokio's own receive spends less CPU per KiB** than giong's `recv` on this benchmark, 6–18% less across the sessions. That gap is giong's own, not `Compat`'s.

Run it with `bench/compat.sh` (the method is at the top of the script, and in [`bench/compat/src/main.rs`](../bench/compat/src/main.rs)); the raw numbers of the latest session are in [`bench/results/compat/`](../bench/results/compat/), the earlier ones in its `sessions/` folder.

## Errors

| Code | When |
|---|---|
| [`GIONG-E1401`](../docs/errors/E1401.md) | A write, flush or shutdown after a buffered send or a shutdown failed (the first call after the failure got the error itself) |
| [`GIONG-E1402`](../docs/errors/E1402.md) | A write after `shutdown` or `close` started |
| [`GIONG-E1403`](../docs/errors/E1403.md) | `write_buffer` given 0 or more than 1 GiB (a panic) |

`giong::error::code(&err)` returns the code, to match on instead of the message.

## License

MIT or Apache-2.0, at your option.
