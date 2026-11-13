# Changelog

All notable changes to giong are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is `0.x`, a minor release (`0.1` → `0.2`) may contain breaking changes.

## [Unreleased]

## [0.3.0] - 2026-11-13

"Prove it": the runtime is checked in ways it was not before, and a service gets the
clients it needs. The cross-thread wake-up is model-checked with loom (which found a lost
wake-up, now fixed), the suite runs on real Linux 6.0 and 6.1 kernels (which found a start
failure on 6.0, now fixed), the HTTP parser is fuzzed every night, a two-hour soak test
looks for leaks, and `giong-tls` had a security review. The first multi-core benchmark
runs from 1 to 10 workers. Three new crates join the workspace, `giong-postgres`,
`giong-mysql` and `giong-redis`, and `giong-http` gains a client. The runtime adds
asynchronous DNS, the client side of TLS, more channels, and error codes that say how to
fix what went wrong.

### Added

- **Error codes.** Every error that giong, giong-http's client, giong-tls, giong-postgres,
  giong-mysql and giong-redis raise starts with a stable code (`GIONG-E0102: no address
  found for db.internal. Fix: …`), says what went wrong and how to fix it, and ends with a
  link to the code's page in [docs/errors](docs/errors/README.md): 65 codes. Functions still return `io::Error`;
  `giong::error::code(&err)` finds the `giong::error::Code` in it, down the chain of
  sources, so programs can match on the code instead of the message. Errors passed on from
  the kernel (connection refused, …) are unchanged and have no code.

- **Verification**: the wake-up handshakes between workers and other threads
  live in `src/notify.rs` and are model-checked with
  [loom](https://github.com/tokio-rs/loom) (`scripts/check.sh loom`, and a new
  CI job). `tests/wake.rs` runs the lost-wake-up race on real hardware.
- **Verification**: coverage-guided fuzzing of `giong-http` with cargo-fuzz: the
  request-head parser, the chunked decoder, and a whole connection served over an
  in-memory transport, in pieces of random size (`giong-http/fuzz/`). CI fuzzes each
  target for 10 minutes every night, keeping the corpus, and briefly on pull requests.
- **Verification**: a soak test (`tests/soak.rs`, `scripts/check.sh soak`): every
  worker serves TCP echo to clients on all workers that close cleanly, abort, half-close
  or cancel receives, alongside UDP, file and cross-worker traffic; afterwards no
  descriptor, memory or driver state may be left over. It runs for two hours every
  night in CI. A one-minute run makes about 1.3 million connections on four workers.
- **Verification**: the whole suite on real Linux 6.0.19 and 6.1.155 kernels, each
  booted in QEMU from an initramfs (`scripts/kernels.sh`), in CI on every change.
- **Benchmarks**: `bench/workers.sh` measures throughput from one worker to N, with CPU
  time per request and how busy the server and the load generator were, on one machine
  or with the load generator on another (`LOAD_HOST`; the Rust servers take
  `BENCH_HOST`).
- **Benchmarks**: the first multi-core results, from 1 to 10 workers on a 14-core
  i5-13500 under WSL2 over loopback, for giong, giong-http, Monoio, raw Tokio and
  Glommio (README, `bench/results/workers-i5-13500-wsl/`). giong had the highest median
  at 1, 2 and 8 workers, was level with Monoio at 4 and behind it at 6, and peaked at
  1.50M requests/s with the load generator as the limit. Scaling efficiency is not
  readable on this setup.
- **`giong-http`**
  - An HTTP/1.1 client, `giong_http::client`, behind the new `client` feature: `Client`
    (one per worker, clones share it) with `get`, `post`, `put`, `patch`, `delete`,
    `head` and `request`, and a `RequestBuilder` for headers, a body, `json` and a
    per-request timeout. A pool of kept-alive connections for each origin, at most 16 by
    default, handed out first come, first served; an idle connection the server closed
    is not used, and an idempotent request that meets a connection closing under it is
    sent once more. HTTPS through giong-tls, verified against the machine's authorities.
    Redirects followed (up to 10; never from HTTPS to HTTP; credentials kept to their
    origin). Every request has a timeout (30 s) and every response a size limit (16 MiB).
    Strict reading: an ambiguous response is an error, and its connection is not used
    again. Errors have the codes GIONG-E1101 to E1109 and say whether a retry can help
    (`Error::is_transient`). On one core, with 64 connections, it sends 1.6 to 1.8 times
    as many requests per second as reqwest, with a lower p99 (`bench/http-client.sh`).
  - A response body of 32 KiB or more is no longer copied after its head: the two go
    out in one vectored write over TCP and Unix sockets. On one worker, a 64 KiB static
    body is served 40% faster and a 1 MiB one 78% faster (an owned 1 MiB body: 2.2
    times as fast); smaller bodies are copied as before, which measured faster for
    owned bodies of 16 KiB. `Transport::write_all_with_body` sends a head and a body;
    its default copies as before, so existing transports keep compiling.
  - `Request::peer_addr`: the client's address, for logging, rate limits and
    access rules. `None` over a Unix socket. Transports report it through the
    new `Transport::peer_addr`, which has a default, so existing transports
    keep compiling.
  - `Server::max_connections`: the most connections one `serve` call (normally
    one per worker) keeps open. At the limit the server stops accepting, and new
    clients wait in the kernel's listen queue instead of being refused.
- [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md): how to allow io_uring in Docker, Docker
  Compose and Kubernetes without turning seccomp off.
- **Sync**
  - `sync::oneshot`: a channel for one value, to hand a result back to the task that
    asked for it, from any worker or thread. `Sender::closed` tells the producer that
    nobody is waiting any more.
  - `sync::mpsc::bounded`: a multi-producer channel that holds at most `n` values.
    `send` waits while it is full, so a fast producer slows down to the consumer's pace;
    waiting senders get room in arrival order, and a cancelled `send` passes its place on.
- **Networking**
  - Vectored writes: `TcpStream::write_vectored` and `write_all_vectored` (and the same
    on `UnixStream`) send several buffers one after the other with one `sendmsg`, up to
    1,024 per call, instead of copying them into one. `IoBuf` is implemented for
    `Cow<'static, [u8]>`, so static and owned pieces can go in the same call.
  - Name resolution that does not block the worker: `net::lookup_host` and
    `net::Resolver`. Reads `/etc/hosts`, then asks the servers in `/etc/resolv.conf`
    with `A` and `AAAA` queries in parallel over UDP, again over TCP when an answer is
    truncated, follows CNAMEs, applies the search list and `ndots` (so Kubernetes
    service names work), fails over to the next server, and caches answers for their
    TTL (at most 5 minutes). Reloads the files when they change.
  - `TcpStream::connect` with several addresses tries them the Happy Eyeballs way
    (RFC 8305): families alternate and each attempt gets 250 ms before the next starts
    alongside it, so an address that never answers no longer costs a TCP timeout.
- **Files**
  - `fs::read_dir` lists a directory: `ReadDir::next_entry` returns `DirEntry`s with
    the name, path, inode number and type (from the directory itself, so no `stat` per
    entry on most filesystems), and `metadata()`. io_uring cannot read directories, so
    each batch of about a hundred entries is a direct `getdents64` call on the worker,
    which yields to other tasks between batches.
  - `File::allocate` (`fallocate`) reserves disk space for a range, growing the file if
    needed, so writes there cannot fail for lack of space.
  - `Metadata::file_type` and `fs::FileType`.
- **`giong-tls`**
  - The client side: `TlsConnector::connect(server_name, tcp)` checks the server's
    certificate against trusted authorities and the name (a DNS name sent through
    SNI, or an IP address). Trust the machine's certificate authorities
    (`with_system_roots`, which honours `SSL_CERT_FILE` and `SSL_CERT_DIR` and knows
    where the common distributions keep their bundle), Mozilla's compiled in
    (`with_webpki_roots`, behind the new `webpki-roots` feature), or your own
    (`with_root_pem`, `with_roots`). `alpn` offers protocols, `client_auth` presents a
    client certificate for mutual TLS, and clones share a session cache so that
    reconnecting resumes the session.
  - `TlsStream::peer_certificates` and `TlsStream::resumed`.
  - `TlsStream::received_close_notify`: whether the peer ended the connection with a
    TLS `close_notify` alert, or the TCP connection merely closed, which anyone on the
    path can cause. Check it before trusting data that ends where the connection ends.
- **`giong-postgres`**, a new crate: a PostgreSQL client and connection pool for giong,
  speaking the wire protocol itself (no libpq, nothing from Tokio).
  - `Client` with `query`, `query_one`, `query_opt`, `execute`, `batch_execute`,
    `transaction` and `cancel_token`. Tasks sharing a connection pipeline their
    statements; every statement is prepared once per connection and kept in a bounded
    cache, and prepared again by itself after a schema change.
  - `Pool`, one per worker: statements that stand alone share the least busy
    connection, `get()` takes one for a transaction and rolls back what is left open
    when it comes back, and new statements avoid a connection stuck behind a slow one.
  - TLS through `giong-tls`, on by default for remote hosts and always verified;
    SCRAM-SHA-256, MD5, and clear-text passwords only over TLS or to this machine.
  - `Config::from_url`, strict about unknown parameters and about `sslmode` values
    that would silently fall back to no encryption.
  - Errors with an `ErrorKind` (each has its giong code, `GIONG-E3001` to `GIONG-E3010`,
    shown in front of the message), the server's SQLSTATE code and every field it sent;
    finite defaults for the connect timeout (10 s), the statement timeout (30 s,
    enforced by the server), the result size (64 MiB), the statement cache (256) and
    the pool wait (5 s).
- **`giong-mysql`**, a new crate: a MySQL 8 client and connection pool for giong, speaking
  the client/server protocol itself (no libmysqlclient, nothing from Tokio), with the same
  shape as giong-postgres.
  - `Client` with `query`, `query_one`, `query_opt`, `execute`, `insert` (returns the
    `AUTO_INCREMENT` id), `batch_execute`, `transaction` and `cancel_token` (`KILL QUERY`).
    Tasks sharing a connection pipeline their statements; every statement is prepared once
    per connection, kept in a bounded cache, and run with the binary protocol. Messages
    over 16 MiB are split and joined both ways.
  - Transactions that cannot end silently: after the server rolls one back (deadlock) or
    commits it (DDL), its later statements and `commit` fail instead of running outside
    it.
  - `Pool`, one per worker, as in giong-postgres.
  - TLS through `giong-tls`, on by default for remote hosts and always verified.
    `caching_sha2_password` (fast path; the password itself only over TLS or a Unix
    socket, or RSA-encrypted with a configured key or, on loopback, the server's) and
    `mysql_native_password`; `sha256_password` and clear text only over TLS.
  - `Config::from_url`, strict about unknown parameters and refusing
    `ssl-mode=PREFERRED`. Sessions run in UTC; session variables can be set when
    connecting.
  - Errors with an `ErrorKind` (`GIONG-E3101` to `GIONG-E3110`), MySQL's error number and
    SQLSTATE; finite defaults for the connect timeout (10 s), the `SELECT` timeout (30 s,
    enforced by the server), the result size (64 MiB), the statement cache (256) and the
    pool wait (5 s).
  - `bench/mysql.sh` compares it with mysql_async and SQLx.
- **`giong-redis`**, a new crate: a Redis client and connection pool for giong, speaking
  RESP3 itself (Redis 6 and later, Valkey; nothing from Tokio).
  - `Client` with `query` for any command (built with `cmd`), read as the Rust type asked
    for (`FromValue`: `Option<String>`, `i64`, `Vec<(String, f64)>`, `HashMap`, tuples,
    `Value`...), and methods for the everyday commands (`get`, `set`, `set_ex`, `set_nx`,
    `del`, `exists`, `expire`, `incr`, `mget`, `hget`, `hset`, `hgetall`, `publish`,
    `ping`). Tasks sharing a connection pipeline their commands.
  - `Pipeline`, made atomic (`MULTI`/`EXEC`) with `atomic()`; `Client::watch` for
    optimistic transactions; `query_blocking` for `BLPOP`, `XREAD ... BLOCK` and the
    other blocking commands. Commands that would change the connection for the other
    tasks sharing it (`MULTI`, `SELECT`, `SUBSCRIBE`, `AUTH`...), and blocking commands
    sent with `query`, are refused before they are sent, with what to use instead.
  - `PubSub`, on a connection of its own, with patterns; waiting for a message can be
    cancelled without losing one.
  - `Pool`, one per worker: commands share the least busy connection, steering away from
    one stuck behind a slow command; `take()` gives a task a connection to itself, and a
    connection left in a blocking command is closed rather than returned.
  - ACL users, TLS through `giong-tls` (`rediss://`, always verified), Unix sockets, and
    `Config::from_url`, strict about unknown parameters.
  - Errors with an `ErrorKind` (`GIONG-E3201` to `GIONG-E3211`) and the server's error
    code (`WRONGTYPE`, `NOSCRIPT`...); finite defaults for the connect timeout (10 s), the
    response timeout (30 s; a late reply is dropped and the connection stays usable), the
    reply size (64 MiB; a larger reply is thrown away as it arrives) and the pool wait
    (5 s).
  - `bench/redis.sh` compares it with redis-rs and fred.

### Changed

- The messages of errors giong raises itself now start with their code and end with a
  link (see Added). Their `io::ErrorKind`s are unchanged. `time::Elapsed` turned into an
  `io::Error` now carries `GIONG-E0007` instead of being a bare `TimedOut`.
- When io_uring is blocked or missing (Docker's default seccomp profile, gVisor, the
  `kernel.io_uring_disabled` sysctl), starting a runtime now fails with an error that says
  so and links to the new [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md), instead of a bare
  "Operation not permitted".
- **Breaking:** `Handle::spawn_on` returns a `RemoteJoinHandle` to the task's result
  instead of a `bool`. The handle is `Send` and `Sync`: await it on any worker or on a
  plain thread, `abort()` it from anywhere (the future is dropped on its own worker), or
  drop it to detach the task as before. A job a stopped worker refuses resolves to
  `JoinError::Cancelled`, where `spawn_on` used to return `false`; a panic in the closure
  or the task resolves to `JoinError::Panic`. The output must be `Send`. A worker index
  out of range now panics with `GIONG-E0008` instead of returning `false`.
- **Breaking:** `TcpStream::connect` takes the new `net::ToSocketAddrs` instead of
  `std::net::ToSocketAddrs`, and resolves host names without blocking. Socket addresses,
  `(ip, port)`, `"host:port"`, `(host, port)` and slices of addresses work as before;
  custom implementations of the standard trait no longer do. `bind` is unchanged.

### Fixed

- A wake-up from another thread could be lost, leaving the task asleep until
  something else woke it. The executor cleared a task's "queued" flag with a
  plain store before polling it; a waker that published new state and then saw
  the flag still set skipped the enqueue, while the poll could read the state
  from before the wake-up. It needs state that is shared without a lock (a
  `Mutex` hides it), and on x86 it hit about 70 in a million such wake-ups. The
  flag is now cleared with a swap, which synchronises with the waker.
- On Linux 6.0 a runtime could fail to start with "giong needs io_uring buffer rings
  (Linux >= 6.0): Bad address", depending on what happened to be mapped next to the
  receive buffer ring: 6.0 pins one page more than the ring needs (fixed in 6.1). The
  ring now has a spare page. Found by running the tests on a real 6.0 kernel.

### Security

- A [security review of giong-tls](docs/reviews/2026-11-giong-tls.md) and of every crate
  that uses it found two issues in code that is new in this release, both fixed before it
  shipped:
  - The `giong-http` client accepted an HTTPS response body that ends where the
    connection ends (no `Content-Length`, not chunked) when the TCP connection closed
    without a TLS `close_notify`, so someone on the path could cut it short. Such a
    response is now an error (`GIONG-E1106`).
  - `giong-mysql` kept part of a packet received after the plaintext greeting and read it
    after the TLS handshake as if it had come through TLS, so someone on the path could
    slip bytes into the encrypted session (the flaw of CVE-2021-23222 in PostgreSQL
    clients). Any byte left over before the handshake now ends the connection.

### Known limitations

- Not run in production yet.
- The multi-core benchmark ran under WSL2 over loopback; scaling on bare metal and a
  physical network are not measured.
- The `giong-tls` review was done by the project's AI agent; an independent review is
  still wanted.
- No HTTP/2, no streaming HTTP bodies, and tasks do not move between workers.

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

[Unreleased]: https://github.com/giong-run/Giong/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/giong-run/Giong/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/giong-run/Giong/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/giong-run/Giong/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/giong-run/Giong/releases/tag/v0.1.0
