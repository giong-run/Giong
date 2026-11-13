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
| An HTTP/1.1 server or client | [`giong-http`](../giong-http/README.md); [`giong-web`](../giong-web/README.md) for routing and JSON |
| TLS | [`giong-tls`](../giong-tls/README.md) |
| PostgreSQL, MySQL, Redis | [`giong-postgres`](../giong-postgres/README.md), [`giong-mysql`](../giong-mysql/README.md), [`giong-redis`](../giong-redis/README.md) |
| HTTP/2 (until `giong-http` has it) | [h2](https://docs.rs/h2) over `Compat`, plain or over `giong-tls` with ALPN `h2` |
| WebSocket | [tokio-tungstenite](https://docs.rs/tokio-tungstenite) (`accept_async`, `client_async`) over `Compat` |
| A protocol framed by length or by line | [tokio-util](https://docs.rs/tokio-util)'s `Framed` with a codec over `Compat` |
| hyper's API in particular | `hyper::{client, server}::conn` over `hyper_util::rt::TokioIo::new(Compat::new(..))` |
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
| hyper 1 (HTTP/1) | hyper's client against a giong-http server, and giong-http's client against a hyper server, with keep-alive and bodies up to 500 KB |
| tokio-util 0.7 | `LengthDelimitedCodec` over a Unix socket (200 frames up to 1 MB) and `LinesCodec` over TCP |

### What does not

`Compat` provides I/O traits, nothing else. A crate does not work through it when it

- **opens its own connections with `tokio::net`** (reqwest, tonic's `transport`, sqlx, redis-rs, most cloud SDKs): it panics with *"there is no reactor running, must be called from the context of a Tokio 1.x runtime"*, or never finishes;
- **uses Tokio's timers or spawns** (`tokio::time::sleep`, `tokio::time::timeout`, `tokio::spawn`): the same panic. hyper's own timeouts need a `hyper::rt::Timer`, which these tests do not set;
- **requires `Send`** (bounds such as `T: Send` or `F: Future + Send + 'static`): `Compat` is not `Send`, because the stream belongs to one worker. The compiler says so.

Some of these crates take a custom connector or executor, and can work with one built on giong, but each has to be tried. Otherwise, run the crate on a Tokio runtime in a thread of its own.

## How it behaves

- **Writes are buffered**, as with `BufWriter` or a TLS stream. `poll_write` copies the bytes and returns; they are sent on `flush`, when the buffer is full, or when a read on the same task is about to wait. Call `flush` (or `shutdown`) where the data must have gone out; every crate written for these traits does. While one buffer is being sent the next one fills, so a long transfer keeps the socket busy.
- **The write buffer is bounded**: `Compat::new(stream).write_buffer(limit)`, 64 KiB by default, from 1 byte to 1 GiB (`GIONG-E1403` otherwise). A busy stream holds up to twice the limit; an idle one holds no buffer.
- **`TCP_NODELAY` is turned on** for TCP streams (plain or under TLS), as every giong crate does. `Compat` already gathers small writes until a flush, and Nagle's algorithm on top of that only delays them: the HTTP/2 test with 100 streams took 2.3 s with it and 21 ms without. `compat.get_ref().set_nodelay(false)` undoes it.
- **A failed send is reported by the next write, flush or shutdown**, with the kernel's own error; after that, writes, flushes and shutdowns fail with `GIONG-E1401`. Writing after `shutdown` (or `close`) fails with `GIONG-E1402`. Reading still works in both cases. A shutdown that finds the connection already reset counts as done, as with Tokio's `TcpStream`.
- **Dropping a `Compat` discards what was written but not flushed**, like dropping a `BufWriter`. Sends in flight are cancelled the way giong cancels any operation: their buffers stay alive until the kernel lets go.
- **One task reads and one task writes at a time.** `tokio::io::split` and `futures::io::AsyncReadExt::split` work, and so does splitting a `Framed` or a WebSocket stream.
- **Cancel safe.** A read that is dropped loses no data. A write either copies bytes and says how many, or copies nothing.
- **Reading after the end of the stream** keeps returning 0 bytes. `into_inner().await` flushes and hands back the stream with the bytes received but not yet read, to go on with giong's own API.

## What it costs

giong reads into buffers from the worker's pool and writes buffers it owns until the kernel is done with them; these traits read into, and write from, memory the caller lends for one call. So `Compat` copies:

- **Reading:** each byte once more than giong's own `recv`, from the pool buffer into the caller's buffer. Through `AsyncBufRead` (`fill_buf`, then `consume` of the whole chunk) there is no extra copy. When a caller takes only part of a chunk, the rest moves to a heap buffer at once, so that a slow reader never holds on to the worker's pool.
- **Writing:** each byte once, into the write buffer, which giong's `write_all` then sends. One small allocation per send (the future that sends it).

Measured with [`bench/compat.sh`](../bench/compat.sh): one TCP stream over loopback, the measured side on one pinned core and the other end on another, the same peer for every variant, 9 runs interleaved. Median, with the range of the 9 runs in brackets. The machine is a 4-vCPU cloud VM (Xeon at 2.3 GHz, Linux 6.18), where bulk transfers over loopback vary a lot from run to run: read the medians, and the spread.

| Workload | giong's own API | `Compat` | `Compat`, `AsyncBufRead` | Tokio |
|---|---|---|---|---|
| Receive 2 GiB (peer writes 256 KiB at a time), MiB/s | 2215 [1390–3059] | 2353 [1451–2661] | 2071 [1424–3120] | 3131 [2395–4388] |
| CPU per KiB received | 376 ns | 331 ns | 411 ns | 145 ns |
| Send 2 GiB from a 64 KiB buffer, MiB/s | 3148 [2116–4340] | 2707 [2386–2878] | | 3097 [2405–4331] |
| CPU per KiB sent | 309 ns | 360 ns | | 314 ns |
| 64-byte request and reply, round trips/s | 33,008 [28,013–38,662] | 32,273 [27,118–38,079] | | 32,213 [28,904–36,759] |
| CPU per round trip | 14.4 µs | 15.0 µs | | 14.8 µs |

CPU is the measured thread's own (user and system, `CLOCK_THREAD_CPUTIME_ID`). What the numbers say:

- **Sending** through `Compat` gives up about 14% of throughput and spends about 17% more CPU per byte than passing an owned buffer to giong's `write_all`. That is the copy: under callgrind, `memcpy` is 96% of the user-space instructions `Compat` runs on this workload. Tokio, which writes straight from the caller's memory, is level with giong here.
- **Request and reply** differ by less than the noise; the CPU per round trip is about 4% higher. Callgrind counts about 1,000 more user-space instructions per round trip (the boxed send, the flush, one more poll), against roughly 14 µs spent mostly in the kernel.
- **Receiving**, `Compat` and giong's own `recv` are within the noise of each other, and both spend more CPU per KiB than Tokio's 64 KiB reads. The reason is in giong 0.4, not in the adapter: when more than eight receives for one stream complete at once, giong copies the rest out of its buffer pool to keep a slow reader from holding it, and a bulk transfer like this one hits that most of the time (1.6 to 1.8 GiB of the 2 GiB, counted with a probe). `Compat`'s own copy disappears in that cost. This table will change when giong's receive path does; run the script again.

Run it with `bench/compat.sh` (the method is at the top of the script, and in [`bench/compat/src/main.rs`](../bench/compat/src/main.rs)); the raw numbers are in [`bench/results/compat/`](../bench/results/compat/).

## Errors

| Code | When |
|---|---|
| [`GIONG-E1401`](../docs/errors/E1401.md) | A write, flush or shutdown after a buffered send failed (the first call after the failure got the kernel's error) |
| [`GIONG-E1402`](../docs/errors/E1402.md) | A write after `shutdown` or `close` |
| [`GIONG-E1403`](../docs/errors/E1403.md) | `write_buffer` given 0 or more than 1 GiB (a panic) |

`giong::error::code(&err)` returns the code, to match on instead of the message.

## License

MIT or Apache-2.0, at your option.
