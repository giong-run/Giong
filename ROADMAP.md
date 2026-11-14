# Roadmap

Where giong is going, in the order we intend to get there. Nothing here has a date:
a milestone is done when its exit criteria are met. [Open an issue](https://github.com/giong-run/Giong/issues) before starting on
an item so the work is not duplicated.

## Goal

A runtime for **network services on Linux** that makes the fewest possible system calls and
keeps tail latency low, by committing fully to io_uring and to one thread per core, and
that still runs, on epoll, where io_uring is blocked.

By 1.0, a team can write and deploy a real REST API (Postgres or MySQL, Redis, calls to
other HTTPS services, HTTPS itself) with giong's crates alone, in an ordinary Docker or
Kubernetes setup. Each crate that replaces a Tokio-based one is measured against it with a
reproducible benchmark (tokio-postgres with deadpool, mysql_async and SQLx, redis-rs,
reqwest, axum), and the README says where giong does not win.

## Agent-first

More and more code is written by AI agents, so giong is built for them to use correctly on
the first try and to fix on their own when they get something wrong. That means removing
vague conveniences and implicit defaults, never safety: the faster code is written, the
tighter the checks around it have to be.

1. **Errors explain themselves.** Every error giong raises carries a stable code
   (`GIONG-E0001`…), says what went wrong and how to fix it, and links to a page about it.
   Functions keep returning `io::Error`; `giong::error::code(&err)` lets code, and agents,
   match on the code instead of the message.
2. **One obvious, correct way.** No macros and no magic. Types strict enough that a mistake
   is a compile error: enums instead of flags and strings, `Duration` instead of a number of
   milliseconds, `#[must_use]` on futures and handles. No implicit defaults: every timeout
   and size limit is finite, documented, and visible in `Debug` output.
3. **Verification is the law.** Tests, cancellation tests, ThreadSanitizer, `loom`, fuzzing
   and soak tests run in CI and block merging; `cargo-semver-checks` guards the API. Machines
   check the work, so human review is not the bottleneck.
4. **Documentation machines can read.** `AGENTS.md` in the repository; `llms.txt` and
   `llms-full.txt` on giong.run, generated from the docs; a runnable example for every public
   API. Doctests are the source of truth.
5. **Observable from the inside.** Per-worker statistics, connection-pool statistics and
   per-route latency, and structured logs as JSON lines. The runtime adds no dependency for
   this; `tracing` is an optional feature.
6. **An MCP server, `giong-mcp`.** Attached to a running service, it lets an agent read the
   state of workers, tasks, connections, pools, statistics and profiles. Read-only, listening
   on localhost or a Unix socket, and only when the service turns it on.

## Non-goals

These are deliberate. Proposals that need them will be declined.

- **Production on other operating systems.** Linux is the only platform giong is tuned and
  measured on, and io_uring stays the main path; the epoll driver (0.4) covers the Linux
  machines where io_uring is blocked, such as Docker's default seccomp profile. Running on
  macOS for development (kqueue) may follow; Windows (IOCP) not before 1.0, and only if
  users need it.
- **Work stealing.** A task that has started stays on its worker, so tasks need not be
  `Send`. Balancing happens where new work starts: `Handle::spawn_balanced` and
  `TcpListener::bind_balanced`.
- **Every workload.** giong covers network services: HTTP, databases, caches and the clients
  they need. It does not try to replace Tokio for everything Tokio is used for.
- **`Send` futures as a requirement.** The API stays usable with `Rc` and `RefCell`.
- **Macros that hide control flow, and unbounded defaults.** See [Agent-first](#agent-first).

## 0.1 — Foundation (done)

Executor, io_uring driver, TCP, UDP, files, timers, thread-per-core runtime, busy-poll
mode, cross-thread channel, cancellation safety, backpressure, tests and benchmarks. See [CHANGELOG.md](CHANGELOG.md).

## 0.2 — A runtime you can serve from (done)

0.2 took the pieces a real service needs first, wherever they stood in the earlier plan,
together with the verification that needed no extra hardware.

- [x] Graceful connection draining on shutdown (`Builder::shutdown_grace`).
- [x] Unix domain sockets.
- [x] Multishot receive for UDP.
- [x] More file operations: metadata, rename, remove, directories.
- [x] `giong-http`: an HTTP/1.1 server library grown out of the example.
- [x] TLS through `rustls` (`giong-tls`, server side), and HTTPS in `giong-http`.
- [x] Run the multi-threaded tests under ThreadSanitizer (nightly), locally and in CI.
- [x] Verify the declared minimum Rust version in CI (1.80, libraries).
- [x] Exercise the ring setup fallbacks by forcing them on a new kernel
      (`GIONG_RING_TIER`).
- [x] Compare against Monoio and Glommio on one core over loopback (see the README).
- [x] A first two-core measurement (the load generator is the limit; see the README).
- [x] Fuzz-style tests for the HTTP parser: 500,000 random inputs for the request parser
      and the chunked decoder in `giong-http`.

## 0.3 — Prove it (done)

The numbers so far come from a 4-vCPU virtual machine over loopback. Before adding more
features, find out what is actually true.

- [x] Benchmark on a multi-core machine: 1 to 10 workers on a 14-core i5-13500 under WSL2,
      over loopback (see the README). The load generator on a separate host and a
      physical network card move to 0.5.
- [x] Measure throughput from 1 to N workers with `bench/workers.sh`. Under WSL2 the
      one-worker baseline is unreliable, so scaling efficiency still needs bare metal
      (0.5).
- [x] Repeat the Monoio and Glommio comparison on several cores (same run). On a
      physical network: 0.5.
- [x] Run the test suite on real 6.0 and 6.1 kernels (in QEMU, in CI; it found that
      6.0 could fail to start a runtime, now fixed).
- [x] Model-check the cross-thread wake-up handshake with `loom` (it found a lost
      wake-up, now fixed).
- [x] Coverage-guided fuzzing (`cargo fuzz`) of the `giong-http` parser, the chunked
      decoder and a whole connection, nightly in CI.
- [x] Add a soak test: hours of connection churn with fd and memory checks (two hours
      every night in CI).
- [x] A security review of `giong-tls` and the crates that use it
      ([report](docs/reviews/2026-11-giong-tls.md); it found and fixed two medium issues).
      It was done by the project's AI agent: a review by an outside person is still wanted
      before anyone relies on `giong-tls` for something that matters.

**Exit criteria:** published multi-core results with their method, and no open memory- or
thread-safety finding.

## 0.4 — Foundations for services (done)

What every client library and service needs before the database and web crates can be
built on it. Worked on alongside 0.3; changes to the runtime's wake-up path wait for 0.3's.

- [x] Asynchronous DNS resolution (`/etc/hosts`, `resolv.conf`, UDP then TCP, a cache per
      worker), and `TcpStream::connect` by name, trying addresses the RFC 8305 way.
- [x] The client side of `giong-tls`: the machine's CA certificates or compiled-in ones,
      ALPN, client certificates.
- [x] A bounded channel and a oneshot channel in `sync`.
- [x] `spawn_on` that returns a handle to the spawned task's result.
- [x] `giong-http`: the client's address in `Request`, and a limit on open connections.
- [x] Vectored writes (`writev`) so a response head and body need no copy.
- [x] A clear error when io_uring is blocked, and a guide to allowing it in Docker and
      Kubernetes ([docs/DEPLOYMENT.md](docs/DEPLOYMENT.md)).
- [x] An epoll driver (moved up from 0.5), chosen by itself when io_uring cannot be set up
      (a seccomp profile, a sandbox, a kernel older than 6.0), or by `Builder::driver` and
      `GIONG_DRIVER`. The worker still blocks in one place that a wake-up from another
      thread can interrupt. The whole test suite passes on both drivers, in CI inside a
      Docker container with the default seccomp profile, and on a real 5.15 kernel; the
      difference in speed is measured in the README.
- [x] Remaining file operations: reading directories, `fallocate`.
- [x] Agent-first groundwork: error codes with their documentation pages and
      `giong::error::code`, `AGENTS.md`, `llms.txt`.
- [x] Load balancing across workers without moving running tasks: `Handle::spawn_balanced`
      (jobs wait in a shared queue for the first idle worker), `TcpListener::bind_balanced`
      (a busy worker hands new connections to a less busy one) and
      `Handle::least_loaded_worker`.

**Exit criteria:** every item above with tests, including cancellation tests for each new
operation.

## giong-postgres 0.1

A new crate: the Postgres v3 protocol on giong.

- [x] Connecting, SCRAM-SHA-256 (with `ring`, already a dependency through rustls), TLS.
- [x] Parameterised queries, prepared statements with a cache, transactions, the common
      data types.
- [x] Pipelining by default: queries from many requests on one core share one write.
- [x] A connection pool per core, with no `Arc` or `Mutex` between threads.
- [x] Per-worker and per-pool statistics as a public API, in `giong` and here.

**Exit criteria:** the test suite passes against real Postgres 14 to 17 in CI, the protocol
parser is fuzzed, and a benchmark compares it with tokio-postgres and deadpool.

## giong-mysql 0.1

A new crate: the MySQL client/server protocol on giong, with the same shape as
giong-postgres.

- [x] Connecting over TCP or a Unix socket, TLS, `caching_sha2_password` (including the
      RSA exchange) and `mysql_native_password`.
- [x] Prepared statements with a cache and the binary protocol, transactions, the common
      data types.
- [x] Pipelining by default, and a connection pool per core.

**Exit criteria:** the test suite passes against real MySQL 8.0, 8.4 and 9 in CI, the
packet parsers are fuzzed, and a benchmark compares it with mysql_async and SQLx.

## HTTP client and giong-redis 0.1

- [x] An HTTP/1.1 client in `giong-http` (feature `client`): keep-alive, a pool per host,
      HTTPS, timeouts, redirects. Benchmarked against reqwest (`bench/http-client.sh`).
- [x] What production traffic needs from it: HTTP proxies (`HTTPS_PROXY`, `NO_PROXY`,
      `CONNECT` tunnels), gzip and deflate bodies with the limit on what they expand to,
      and bodies streamed piece by piece.
- [x] The database and client parsers fuzzed (`*/fuzz/`), and the database crates tested
      against PostgreSQL 14 to 17, MySQL 8.0, 8.4 and 9.1, Redis 6.2 to 7.4 and Valkey 8 in
      CI.
- [x] `giong-redis`, a new crate: RESP3, automatic pipelining, a pool per core, pub/sub.
      Benchmarked against redis-rs and fred (`bench/redis.sh`). RESP2-only servers (Redis 5
      and older) are not supported: RESP3 has been in every Redis since 6.0 (2020).

**Exit criteria:** the client fetches from a public HTTPS API, `giong-redis` passes against
a real Redis 7 in CI, and benchmarks compare them with reqwest and redis-rs.

## giong-web 0.1 and giong-mcp 0.1 (done)

- [x] Streaming request and response bodies in `giong-http`: `Handler::stream_body` and
      `BodyStream` for requests, `Body::channel` for responses, backpressure both ways;
      `Router::stream_bodies` in `giong-web`.
- [x] `giong-web`, a new crate: a router with path parameters, middleware (logging,
      timeouts, CORS, request ids, compression), JSON through `serde` behind a feature.
      Errors are problem+json bodies with giong codes (`GIONG-E1201` to `E1213`).
      Benchmarked against axum (`bench/web.sh`).
- [x] Structured logs as JSON lines (`giong_web::middleware::Logger`), and an optional
      `tracing` feature (`giong_web::middleware::Trace`, a span per request with
      OpenTelemetry's field names).
- [x] A complete example REST API (`examples/rest-api`: CRUD on PostgreSQL, a Redis
      cache, HTTPS), and the same API on axum to measure it against
      (`bench/rest-api.sh`).
- [x] `giong-mcp`, a new crate: the MCP server described under
      [Agent-first](#agent-first). Read-only, on loopback or a Unix socket, started by
      `GIONG_MCP`; tools `overview`, `workers`, `tasks`, `sources` and `profile`, served
      from `giong::observe` (per-worker counters, tasks named by the line that spawned
      them, poll profiles, pools as sources). It names the task a blocked worker is stuck
      in.

**Exit criteria:** a complete example REST API (CRUD on Postgres, a Redis cache, HTTPS) in
the repository, and a benchmark of it against the same API on axum.

## 0.5 — HTTP/2 and adapters

- [ ] Carried over from 0.3 and 0.4: run `bench/workers.sh` on bare-metal Linux with the
      load generator on a second machine over a physical network card, for scaling
      efficiency and the Monoio and Glommio comparison on a real network (two machines
      rented by the hour are enough).
- [x] HTTP/2 in `giong-http`'s server, over TLS with ALPN and over plain TCP with prior
      knowledge, for the same handlers (`giong-web` included). Hardened against rapid reset,
      frame and CONTINUATION floods, HPACK bombs and readers that stall, with coded errors
      (`GIONG-E1115` to `E1118`); `h2spec` in CI; HPACK and whole connections fuzzed.
- [x] HTTP/2 in `giong-http`'s client, chosen through ALPN or with prior knowledge: one
      connection per origin for every request, bounded by flow control, refused requests
      sent again; tested against the server and against servers that misbehave on
      purpose, fuzzed, benchmarked against reqwest.
- [ ] Adapters for the `futures-io` / Tokio I/O traits, so protocol crates that are generic
      over the transport can run on giong.
- [ ] Decide, based on demand, whether a hyper integration is worth maintaining.

**Exit criteria:** an HTTPS server serving real browsers over HTTP/2, and `h2spec` passing.

## 0.6 — Speed

Each item lands only with a benchmark showing what it buys.

- [ ] Registered ring fd and fixed files (direct descriptors) to skip fd lookups.
- [ ] Zero-copy send for large responses.
- [ ] Receive bundles (several buffers per completion).
- [ ] Optional `SQPOLL` mode.
- [ ] Optional NAPI busy polling.
- [x] Kernel TLS (kTLS) for `giong-tls`: +36% serving 1 MiB files over HTTPS with `sendfile`
  on bare metal; slower than rustls from memory, so off by default
  ([numbers](giong-tls/README.md#when-it-pays-off)).

**Exit criteria:** a documented, reproducible gain for every option that is merged.

## 1.0

A stable API. Requires: a review of every public item against the
[Agent-first](#agent-first) principles, `cargo-semver-checks` in CI, every public item
documented, the limitations section of the README reduced to deliberate non-goals, and at
least one production user reporting back.

WebSocket and gRPC come later, when someone needs them.
