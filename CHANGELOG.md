# Changelog

All notable changes to giong are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is `0.x`, a minor release (`0.1` → `0.2`) may contain breaking changes.

## [Unreleased]

### Added

- **`giong-http`**
  - `Request::peer_addr`: the client's address, for logging, rate limits and
    access rules. `None` over a Unix socket. Transports report it through the
    new `Transport::peer_addr`, which has a default, so existing transports
    keep compiling.
  - `Server::max_connections`: the most connections one `serve` call (normally
    one per worker) keeps open. At the limit the server stops accepting, and new
    clients wait in the kernel's listen queue instead of being refused.

## [0.2.1] - 2026-11-12

A patch release: a file descriptor leak at shutdown, and the logo on crates.io.

### Fixed

- Drain pending file and socket closes before cancelling I/O at runtime shutdown
  to prevent file descriptor leaks
  ([#4](https://github.com/giong-run/Giong/issues/4), thanks to
  [@alejandrodev-vn](https://github.com/alejandrodev-vn)).
- The README logo now uses absolute URLs, so its dark-mode variant also loads on
  crates.io (relative paths only resolve on GitHub).

## [0.2.0] - 2026-11-12

giong becomes a workspace of three crates: the runtime, an HTTP/1.1 server library and
TLS. The runtime gains graceful draining, Unix sockets, multishot UDP and the rest of the
file operations, and is now checked under ThreadSanitizer and on the io_uring setups of
older kernels.

### Added

- **`giong-http`** (new crate): a small, strict HTTP/1.1 server. One async handler; bodies
  by `Content-Length` or chunked; keep-alive and pipelining; limits on head and body size;
  timeouts for idle connections, slow requests and slow readers; graceful shutdown.
  Requests a proxy in front could read differently (two `Content-Length` headers,
  `Content-Length` with `Transfer-Encoding`, bare LF, folded headers) are refused with
  `400`. `Server::serve` for TCP, `Server::serve_unix` for Unix sockets, `Server::serve_tls`
  for HTTPS behind the `tls` feature, and the `Transport` trait with
  `Server::serve_connection` for anything else.
- **`giong-tls`** (new crate): server-side TLS 1.2 and 1.3 on rustls with `ring`.
  `TlsAcceptor` and `TlsStream`; receiving is cancel safe, and a cancelled write poisons
  the sending side of the connection instead of corrupting the record stream.
- **Runtime**
  - `Builder::shutdown_grace`: after a worker's main future returns, its remaining tasks
    get this long to finish the request they are working on. `is_shutting_down()` tells a
    task that it is time to stop taking new work.
  - `GIONG_RING_TIER=coop|basic` forces the io_uring setup used on kernels older than 6.1,
    so those paths can be tested on a new kernel.
- **Networking**
  - `UnixListener` and `UnixStream`, with the same interface as their TCP counterparts,
    plus `UnixStream::pair`.
  - `UdpSocket::recv_from` and `recv` on multishot `recvmsg` with the provided buffer
    ring: cancel safe, one submission for the life of the socket, datagrams returned as a
    `Datagram` borrowed from the worker's pool (`Datagram::is_truncated` reports one that
    was longer than a pool buffer). `UdpSocket::set_broadcast`.
- **Files**
  - `File::metadata`, `fs::metadata` and `fs::symlink_metadata` (`statx`), with `Metadata`
    (`len`, `is_file`, `is_dir`, `is_symlink`, `mode`, `modified`).
  - `fs::read`, `fs::write`, `fs::rename`, `fs::remove_file`, `fs::remove_dir`,
    `fs::create_dir`, and `OpenOptions::custom_flags`.
- **Verification**
  - `scripts/check.sh`: the whole suite repeated in release mode, on all three io_uring
    setup tiers, and under ThreadSanitizer. CI runs all of it, plus a build with Rust 1.80.
  - Sanitizer builds (`--cfg giong_tsan`) close descriptors with `close(2)` instead of
    through the ring, which ThreadSanitizer cannot see and reported as races on reused
    descriptor numbers.
  - 169 tests across the three crates (87 in 0.1).
- **Benchmarks**: `giong-http` in `bench/run.sh`; a two-core run; `bench/tls.sh` (HTTPS
  against Axum + rustls) and `bench/fs.sh` (file I/O against Tokio and blocking `std`).
  Every benchmark server now starts one worker per CPU it is allowed to run on.
- **Documentation**: logo and brand assets in `assets/`; the README shows the logo for
  light and dark themes, and the API documentation uses the giong icon.

### Changed

- **Breaking:** `UdpSocket::recv_from(buf)` and `recv(buf)`, which read into a buffer of
  the caller's, are now called `read_from(buf)` and `read(buf)`, matching
  `TcpStream::read`. The names `recv_from()` and `recv()` now belong to the multishot,
  pool-buffer variants, matching `TcpStream::recv`. The two kinds must not be mixed on one
  socket: after `recv_from`, `read_from` fails with `InvalidInput`.
- The benchmark's `giong` entry is `bench/giong`, the same program as the Monoio, Tokio and
  epoll servers; `examples/http.rs` is no longer measured. All recorded results were
  measured again on a 4-vCPU machine.

### Known limitations

- A one-off read that is dropped (`TcpStream::read`, `UdpSocket::read_from`,
  `File::read_at` on a pipe or device) can still take data. Use `recv` and `recv_from`
  where a read has to be cancel safe.
- No real 6.0 kernel was available: the fallback setups are exercised by forcing them on
  6.18.
- `giong-tls` has no client side and has not had an independent security audit; the
  cryptography and the protocol are rustls's.
- Still no asynchronous DNS, no HTTP/2 and no streaming HTTP bodies.

## [0.1.0] - 2026-11-11

First release: a thread-per-core async runtime for Linux on io_uring, with TCP, UDP, files,
timers and cross-thread messaging. Not production-tested; see the limitations in the README.

### Added

- **Runtime**
  - `block_on` for a single-threaded runtime; `Builder` / `Runtime::run` for one worker
    thread per CPU, each pinned to its core with its own io_uring, run queue, timers and
    receive buffer pool.
  - `Handle` (`shutdown`, `is_shutdown`, `spawn_on`, `workers`), `handle()`, `worker_id()`.
  - Graceful shutdown: `shutdown_signal()`, `until_shutdown(fut)`, and
    `Builder::shutdown_on_signals` for SIGINT / SIGTERM.
  - A panic in a worker's main future stops the runtime and is propagated to the caller.
  - `Builder::spin_before_park`: an opt-in busy-poll mode that trades CPU for latency. A
    worker watches its ring for a while before sleeping, so it reacts to a request without
    a wake-up from the kernel.
- **Tasks**
  - `spawn` for `!Send` futures, `JoinHandle` (await, `abort`, `is_finished`), `JoinError`,
    `yield_now`.
  - A panic inside a task is caught and reported through its `JoinHandle`.
- **Networking**
  - `TcpListener` (`bind` with `SO_REUSEPORT`, `accept`, `from_std`) on multishot accept.
  - `TcpStream` (`connect`, `recv`, `read`, `write`, `write_all`, `shutdown`,
    `set_nodelay`, `peer_addr`, `local_addr`, `from_std`).
  - `recv` uses multishot receive with a kernel-provided buffer ring and returns a
    `RecvBuf` borrowed from the worker's pool.
  - `UdpSocket` (`bind`, `connect`, `send`, `recv`, `send_to`, `recv_from`, `from_std`),
    IPv4 and IPv6.
  - Ownership-passing buffer traits `IoBuf` / `IoBufMut`, implemented for `Vec<u8>`,
    `String`, `Box<[u8]>`, `Rc<[u8]>`, `Arc<[u8]>`, `&'static [u8]` and `&'static str`.
- **Files**
  - `fs::File` and `fs::OpenOptions`: open, positional `read_at` / `write_at` /
    `write_all_at`, `sync_all`, `sync_data`, `close`. Every operation goes through the
    ring; there is no blocking thread pool.
- **Time**
  - `sleep`, `sleep_until`, `timeout`, backed by a six-level hierarchical timing wheel
    with 1 ms resolution. Timers never fire early.
- **Sync**
  - `sync::mpsc` unbounded channel, usable across workers and from threads outside the
    runtime.
- **Safety and backpressure**
  - Cancellation safety: when an in-flight operation's future is dropped, its buffer is
    kept alive by the driver until the kernel confirms the cancellation.
  - `recv` and `accept` are cancel safe: wrapping them in `timeout` loses no data and no
    connection.
  - A slow or stalled reader cannot starve other connections: its multishot receive is
    paused and its backlog is moved off the shared pool.
  - A multishot accept pauses once 1024 accepted connections are waiting.
  - On shutdown every operation is cancelled and awaited before memory is released.
- **Examples**: `echo`, `http` (HTTP/1.1 with keep-alive, pipelining, idle and write
  timeouts, size limits), `udp_echo` and `cat`.
- **Benchmarks**: `bench/run.sh` (throughput), `bench/scale.sh` (one connection to
  thousands, with CPU time per request) and `bench/syscalls.sh`, with seven reference
  servers (Node.js, Bun, Axum, raw Tokio, hand-written epoll, Monoio, Glommio) and the
  recorded results.
- **Tests**: 62 tests covering the executor, timers, TCP, cancellation, buffer exhaustion
  and multi-worker behaviour; `scripts/valgrind.sh` for a memcheck pass.

### Known limitations

- No Unix sockets and no asynchronous DNS (`bind` and `connect` resolve names blocking).
- No TLS and no HTTP/2.
- Not compatible with crates that require the Tokio runtime.
- Tasks never migrate between workers.
- Shutdown drops remaining connections; it does not wait for requests in progress.
- Not yet measured on more than one core or on a physical network.
- The declared minimum Rust version (1.80) and the fallback paths for kernel 6.0 have not
  been exercised yet.

[Unreleased]: https://github.com/giong-run/Giong/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/giong-run/Giong/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/giong-run/Giong/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/giong-run/Giong/releases/tag/v0.1.0
