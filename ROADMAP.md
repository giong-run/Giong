# Roadmap

Where giong is going, in the order we intend to get there. Nothing here has a date:
a milestone is done when its exit criteria are met. [Open an issue](https://github.com/giong-run/Giong/issues) before starting on
an item so the work is not duplicated.

## Goal

**The best async runtime for Rust, on technical merit**: a complete, general-purpose runtime
that fits any workload a service has, faster and leaner than Tokio, judged by measurements
and not by adoption. Linux first: it makes the fewest possible system calls and keeps tail
latency low by committing fully to io_uring and to one thread per core, and it still runs,
on epoll, where io_uring is blocked.

giong is a late entrant, built for a time when AI agents write much of the code. Its API
does not have to be short to type; it has to be strict, always correct, and able to grow
for many years. After 1.0, giong stays on major version 1 and grows through 1.x minor
releases (see [Agent-first](#agent-first) for the rules that make this possible). It
replaces the Tokio ecosystem with crates of its own (HTTP server and client, TLS, DNS,
database drivers) rather than adapting to it.

By 1.0, a team can write and deploy a real REST API (Postgres or MySQL, Redis, calls to
other HTTPS services, HTTPS itself) with giong's crates alone, in an ordinary Docker or
Kubernetes setup. Each crate that replaces a Tokio-based one is measured against it with a
reproducible benchmark (tokio-postgres with deadpool, mysql_async and SQLx, redis-rs,
reqwest, axum), and the README says where giong does not win.

After 1.0, the 1.x releases add gRPC, macOS, HTTP/3 and Windows, then a memory limit kept
under any load and a developer kit, the `giong` command, one at a time and without breaking the 1.0 API (see the milestones after 1.0
below). Alongside them, a third track fills in what a general runtime still lacks: blocking
and compute work, processes and signals, structured concurrency, and scheduling that keeps
a small machine efficient (see [Runtime core](#runtime-core-track-r-after-10)). Linux stays
the platform giong is tuned and measured on.

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
   and size limit is finite, documented, and visible in `Debug` output. Every addition to
   the API, through 1.0 and all of 1.x, follows four rules:
   1. One way to do each thing.
   2. Misuse fails at compile time, or at run time with a coded error.
   3. Every behaviour code relies on is documented.
   4. Releases only add; they never change what existing code means.
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

- **The same tuning on every operating system.** Linux is the platform giong is tuned and
  measured on, and io_uring stays the main path; the epoll driver (0.4) covers the Linux
  machines where io_uring is blocked, such as Docker's default seccomp profile. macOS (1.2)
  is for developing and testing on a Mac, and Windows (1.4) for services that have to run
  there. Both will pass the same test suite, but speed work lands on Linux first, and
  neither comes before 1.0.
- **Work stealing.** A task that has started stays on its worker, so tasks need not be
  `Send`. Balancing happens where new work starts: `Handle::spawn_balanced` and
  `TcpListener::bind_balanced`.
- **Everything in one crate.** giong is a runtime: it takes only what a runtime alone can do
  (scheduling, I/O, time, processes, memory, observation). Protocols, frameworks and tools
  are crates built on top of it, as giong-http, giong-web and the database drivers are.
  Network services come first and set the priorities, but blocking work, CPU-heavy work and
  child processes have a place in the runtime too (see
  [Runtime core](#runtime-core-track-r-after-10)).
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

## 0.5 — HTTP/2 and adapters (done)

- [x] Carried over from 0.3 and 0.4: `bench/workers.sh` on bare-metal Linux with the
      load generator on a second machine: two EPYC 4345P (8 cores) over 25 GbE, 1 to 16
      workers, with Monoio, Glommio and Tokio
      ([results](bench/results/bare-metal-epyc-4345p/); see the README).
- [x] HTTP/2 in `giong-http`'s server, over TLS with ALPN and over plain TCP with prior
      knowledge, for the same handlers (`giong-web` included). Hardened against rapid reset,
      frame and CONTINUATION floods, HPACK bombs and readers that stall, with coded errors
      (`GIONG-E1115` to `E1118`); `h2spec` in CI; HPACK and whole connections fuzzed.
- [x] HTTP/2 in `giong-http`'s client, chosen through ALPN or with prior knowledge: one
      connection per origin for every request, bounded by flow control, refused requests
      sent again; tested against the server and against servers that misbehave on
      purpose, fuzzed, benchmarked against reqwest.
- [x] Adapters for the `futures-io` / Tokio I/O traits, so protocol crates that are generic
      over the transport can run on giong: `giong-compat`, tested with tokio-rustls,
      futures-rustls, tokio-tungstenite, h2, hyper's HTTP/1 connection API and tokio-util's
      codecs, and measured against giong's own API and Tokio (`bench/compat.sh`).
- [ ] Decide, based on demand, whether a hyper integration is worth maintaining. Not
      decided: no one has asked yet, and hyper's connection API already runs through
      `giong-compat`. Revisited when someone needs more of hyper.

**Exit criteria:** an HTTPS server serving real browsers over HTTP/2, and `h2spec` passing.
Met: Chromium 153 loads the page of `giong-http`'s `h2` example and eight downloads beside
it over HTTP/2, chosen through ALPN, and `h2spec` passes all 147 cases in CI.

Speed work shipped in 0.5.0 too, each item with a benchmark showing what it buys:

- [x] Registered ring fd: about 11 ns saved of ~130 per `io_uring_enter`, which no
  benchmark of a whole server can see (neutral on bare metal); on by default from 5.18.
  Fixed files (direct descriptors) are not done: they would save about 11.5 ns per send,
  0.6% of a request, less than keeping the descriptor table costs.
- [x] Zero-copy send for large responses: on bare metal, CPU time per request falls by 16%
  at 64 KiB, 62% at 256 KiB, 68 to 72% at 1 MiB and 85 to 86% at 16 MiB, at the same
  request rate (the link is full); bytes that cannot change only.
- [x] Receive buffers filled piece by piece (Linux 6.12): memory at 20 workers 49.9 → 23.4
  MiB, requests per second unchanged.
- [x] Optional `SQPOLL` mode: tried and not merged. With its threads on CPUs of their own it
  lost on bare metal (8 workers and 8 kernel threads: 2.32M req/s against 2.41M for 8
  workers alone, 73% more CPU per request, since the threads spin).
- [x] Kernel TLS (kTLS) for `giong-tls`: +36% serving 1 MiB files over HTTPS with `sendfile`
  on bare metal; slower than rustls from memory, so off by default
  ([numbers](giong-tls/README.md#when-it-pays-off)).

## 0.6 — Reviewed (done)

The first gate on the way to 1.0. No new features: everything written so far was reviewed,
and what the reviews found was fixed.

- [x] A review of every crate by a reviewer that had not written it: the drivers and the
      runtime; sockets, files and channels; `giong-http`; `giong-web`; `giong-tls`;
      `giong-compat`; `giong-postgres`; `giong-mysql`; `giong-redis`; `giong-mcp`.
- [x] A review of all `unsafe` code and of the workspace's security, the release workflow
      included. Miri runs the tests it can in CI, and clippy checks that every `unsafe`
      block says why it is sound.
- [x] The findings fixed, in the 0.6.0 [changelog](CHANGELOG.md). Those that need a
      decision about the API wait for the API freeze.
- [x] Fuzz targets where the reviews found none: DNS answers and `resolv.conf`, the HTTP
      client reading a response, whole PostgreSQL, MySQL and Redis sessions, the records
      kernel TLS reads, and `giong-mcp`'s requests.

The reviewers were the project's AI agents. A review by an outside person is still wanted.

**Exit criteria:** no open finding rated critical or high. Met.

## 0.7 — API settled (done)

The second gate on the way to 1.0: the API that 1.0 keeps, decided item by item, with the
breaking changes made now rather than after 1.0.

- [x] Every public item weighed against the [Agent-first](#agent-first) principles, with
      the items the 0.6 reviews and the [portability check](docs/reviews/2026-11-api-portability.md)
      left for it: each one changed, kept on purpose, or left for 1.x as an addition, with
      its reason ([decisions](docs/reviews/2026-11-api-freeze.md)).
- [x] A service built by an AI agent from the 0.6.0 documentation alone. The gaps it found
      in the documentation are filled, and the API changes it called for are among the
      decisions.
- [x] The breaking changes made, in the 0.7.0 [changelog](CHANGELOG.md), each with how to
      migrate.

**Exit criteria:** no item left for the freeze undecided. Met. The freeze is declared once
real use in a service has reported back, which may still add items before 1.0.0-rc.1.

## 0.8 — Verified in CI (done)

The verification that needs no machines of its own, run on every change. No new public
features.

- [x] `cargo-semver-checks` in CI, checking every change against 0.7.0.
- [x] Speed and memory thresholds checked in CI: instructions per request, counted so the
      result is the same on every run of a shared runner, heap allocations per request, and
      memory per idle connection over HTTP/1.1, HTTP/2 and HTTPS, each with its raw data in
      `bench/`.
- [x] Kani proofs for the small `unsafe` core (the slab, the buffers, the kernel-owned
      boxes, the lifetimes of operations), run in CI.
- [x] Fault injection through an internal simulation (`sim/`): lost, reordered and delayed
      packets, reset connections, failing disks and jumping clocks, chosen by a seed, and a
      failing run replayed exactly from its seed; long runs in simulated time, months of
      uptime on every CI run. It covers Gióng's own code on the epoll driver, not io_uring,
      the kernel or the NIC. Internal only; a public form is a 1.x addition.
- [x] Every fuzz target run in the nightly fuzz workflow with no open crash.
- [x] The remaining code fixes from the service built from the documentation alone: a log
      line and a span event for a request dropped before its response (F16), plainer type
      names and articles in messages, and a cause said once (F15), and giong-mcp saying
      when a blocked task is a giong-http connection's request handler (F2; naming the
      handler itself needs a request label, a 1.x addition). The other findings were
      settled in 0.7 or left for 1.x as additions, in the
      [decisions](docs/reviews/2026-11-api-freeze.md).
- [x] Memory bounded under attack: the requests a worker receives in full share a budget
      (`max_buffered_request_bytes`), and CI runs hundreds of slow uploads against a server
      in a 128 MiB container on every change.

**Exit criteria:** all of the above green on `main`. Met in 0.8.0.

## 0.9 — Bare metal and real use

What only real machines and a real service can show. No new public features.

- [x] A 72-hour soak on bare metal: both drivers ran 72 hours with no leak, crash or stall
      (`bench/results/bare-metal-epyc-4345p/linux-7.0/soak-72h/`, #155).
- [x] Benchmarks on bare metal against Tokio and the other runtimes, compared with 0.5.0's
      pass, with method and raw data in the repository; the README and giong.run take their
      numbers from them (`bench/results/bare-metal-epyc-4345p/linux-7.0/`, #155).
- [x] UDP segmentation offload measured in the same rental, as input for HTTP/3 (no API):
      sending within 1.4% of the kernel's own, receive offload not verified there
      (`post-soak/two-host/gso/`).
- [x] The server held to the cheapest class of machine, 1 CPU and 512 MB (a container on
      the rental), against Tokio, Go and nginx: requests per second, idle HTTPS connections
      held, and memory per idle connection with kernel TLS, which a virtual machine cannot
      measure. Input for 1.5 (`small-box/`).
- [x] Real use: giong in a service, with its user reporting back. The author's own service
      moved from 0.8.0 to 1.0.0-rc.1 and reported no error (2026-11-20); its earlier
      feedback is issue #154, whose fix is #156.

**Exit criteria:** the soak ends with no leak, crash or stall, and the service's user
reports no blocker. Met. 0.9.0 itself was never published: its work shipped in 1.0.0-rc.1.

## 1.0

A stable API. No features are added between 0.6 and 1.0; what is left is a series of
gates (0.6, 0.7, 0.8, 0.9), and 1.0.0 follows release candidates (`1.0.0-rc.N`) that pass
them all.

- [x] Documentation: every public item documented, and the limitations section of the
      README reduced to deliberate non-goals and a short list of missing pieces (Unix
      datagram sockets, multicast, `set_len`, file locks) that 1.x can add without a
      breaking change.
- [x] 0.8 and 0.9 passed.
- [x] `1.0.0-rc.N`: release candidates that change nothing but fixes, until one passes
      every gate; that one becomes 1.0.0. 1.0.0-rc.1 passed them, and 1.0.0 is its code
      with the version changed.

Before the freeze, the behaviour callers rely on is settled, and each commitment has a test
that fails without it. 1.0 stays small: nothing here adds an API.

- [x] **What a cancelled operation does**, written once for every I/O API in
      [STABILITY.md](docs/STABILITY.md#cancellation): the operation may still happen, the
      driver keeps buffers and descriptors until the kernel is done with them, slots come
      back when the operation ends, and that is the earliest moment a 1.x budget gives
      their share back. *Proved by* `tests/cancel_docs.rs` (every public async fn says
      whether it is cancel safe) and the two tests below.
- [x] **A dropped `File::close` still closes**, on both drivers: a close is never
      cancelled, only its result can be lost. *Test*: on epoll, the blocking threads are
      kept busy so the close waits in their queue; on io_uring, the close is sent to the
      kernel's worker thread (a test-only switch setting `IOSQE_ASYNC`, since a close on a
      local filesystem completes at once and cannot show the bug). In both, the future is
      polled until the close is submitted, then dropped; once the driver has finished, no
      descriptor of the process refers to the file any more (`/proc/self/fd`, which a
      reused descriptor number cannot fool). `a_dropped_close_still_closes_on_the_epoll_driver`
      in `tests/cancel.rs` and `a_dropped_close_still_closes` in `src/driver/uring.rs`;
      both failed before the fix (#159).
- [x] **A PostgreSQL transaction refuses to commit after a statement in it was given up**,
      as giong-mysql's already does: `commit` returns a coded error and the transaction
      has to be rolled back. *Test*, on the PostgreSQL server CI starts: begin, insert a
      row, send a statement and drop its future before the answer, check that `commit`
      fails with the code, roll back, and check from a second connection that the row is
      not there. A statement whose answer had arrived before its future was dropped does
      not block the commit. The same test runs against giong-mysql.
      `a_statement_given_up_inside_a_transaction_stops_the_commit` in both crates'
      tests (#158).
- [x] **Defaults callers rely on**, in [STABILITY.md](docs/STABILITY.md#defaults-you-can-rely-on):
      whether each limit waits or refuses by default, and the rule that 1.x never changes
      which; a new early refusal (overload, tenant budget) is a setting that is off by
      default. *Proved by* the slow-uploads and idle-connections CI jobs (`E1120`,
      `E1121`) and the pools' wait-timeout tests. The names of 1.x's budget types, its
      ledger and how it shares quotas wait for the prototype of 1.5.
- [x] **The release candidate says what it was verified on**: the rc.1 release pull
      request names the soaked commit, the release commit and the change between them,
      which tests ran again on the release commit, and the soak that change got
      ([RELEASING.md](docs/RELEASING.md#release-candidates-of-10)). A soak is sized by what
      changed: none outside the soak paths, 2 to 4 hours for the drivers, the scheduler or
      a cancellation path, 72 hours for a new major version, driver or scheduler. rc.1
      stands on 0.9's 72 hours (tree `f1b23074`), and the changes since (#134's bind, the
      channel, UDP, close and TLS receive fixes) get 4 hours per driver once they have all
      merged: run on `892d904`, about 320 million connections per driver
      (`bench/results/bare-metal-epyc-4345p/linux-7.0/rc-short-soak/`, #169). rc.1 differs from that commit in `src` only by the
      removed aliases and the bounded channel's type names. TLS has its own soak since
      #171 (`bench/results/tls-soak/`).
- [x] **The removals**: the checklist in
      [STABILITY.md](docs/STABILITY.md#changes-made-before-10), in one release pull request.

The API freeze also makes sure that nothing public ties the API to Linux or to HTTP/2 in a
way that the releases below would have to break. A first pass over `main` found nothing
that forces a 2.0, and a short list to settle before the freeze
([findings](docs/reviews/2026-11-api-portability.md)).

After 1.0, each minor release has one main theme and breaks nothing: code written for 1.0 keeps
compiling. The protocol and platform work runs on two tracks at once (a third, the runtime
core, is described after 1.5), and each release ships as soon as its own
gates pass. The protocol track does gRPC, which builds on HTTP/2, then HTTP/3, the largest
piece of protocol work. The platform track first moves the system calls outside the drivers
behind one internal layer, built once for both systems, then adds macOS, whose kqueue driver
follows the path the epoll driver opened, then Windows, the largest change of platform.
gRPC is 1.1 and macOS 1.2; HTTP/3 and Windows take 1.3 and 1.4 in the order they pass their
gates. Nothing of 1.x lands on `main` before 1.0.0: until then only prototypes and
measurements that touch no released code, and 1.0.x fixes come from a `release/1.0` branch
afterwards. 1.5 follows on its own: it needs 1.2's overload protection and nothing from the
others. WebSocket comes later, when someone needs it.

Every 1.x release keeps the [Agent-first](#agent-first) line (coded errors with their
pages, one obvious way, `llms.txt` and `AGENTS.md` updated with the feature, giong-mcp and
tracing for what it adds), passes a build of a small service by an AI agent from the
documentation alone, and is benchmarked against the reference implementation named in its
section before any claim about speed.

Two speed items once planned for 0.6 also wait until after 1.0, and land only with a
benchmark showing what they buy: receive bundles (several buffers per completion) and
optional NAPI busy polling. Two more for large machines, measured on a server with two
sockets: workers that use the memory of their own NUMA node, and workers matched to the
receive queues of the network card, so a packet is handled on the core it arrived on.
giong already sizes and pins its workers by itself (CPU affinity and container quota) and
spreads connections between them; it does not retune itself while running, which would
make behaviour hard to predict, but giong-mcp suggests settings from what it measures.

## 1.1 — gRPC (done)

Released as 1.1.0 on 2026-11-21. Shipped: giong-grpc, giong-proto and giong-grpc-build,
health and reflection, interop with grpc-go, trailers and streamed request bodies, and
`Client::preconnect` and `ping_idle_http2` from issue #154. The open items below move to a
later 1.x.

gRPC on `giong-http`'s HTTP/2 server and client, in a new crate, `giong-grpc`, on a
protobuf of its own, and a simulation driver that tests every service deterministically.

- [x] Trailers in `giong-http`: a streamed response body that ends with trailers, and
      trailers that can be read at the end of a request body and of a client response. gRPC
      sends its status in them.
- [x] A client request body sent as it is produced, and a response that can be read while
      the request body is still being sent, for client-streaming and bidirectional calls.
      The server already keeps reading a request body while its answer streams; this
      milestone adds the test that holds it to that.
- [x] `giong-grpc`: unary, server-streaming, client-streaming and bidirectional calls;
      deadlines (`grpc-timeout`), status codes, metadata, cancellation, message size limits
      and gzip. Code generated from `.proto` files at build time, with no macros in user
      code and no `protoc` to install. Errors carry giong codes.
- [x] `giong-proto`: protobuf messages decoded as views over the receive buffer, with no
      copy and no allocation for strings and bytes, and encoded straight into the send
      buffer. Held to Google's protobuf conformance suite and fuzzed against prost.
- [ ] The same service over gRPC-Web, for browsers, and as JSON over HTTP from the
      `google.api.http` annotations of its `.proto`: one definition, three ways to call it.
- [ ] The HTTP client, from what a service gateway asked for (issue #154): connections
      opened ahead of the first request to an origin (preconnect), HTTP/2 keep-alive
      `PING`s that find a dead connection before a request does, and hooks that report
      the pool's and each request's numbers (waiting, dialing, reused, time to first
      byte) to the service's own metrics. One dial shared by the requests that start
      together for an origin is #156, outside this list.
- [ ] A simulation driver: a service, its clients and the network run in one thread, in
      simulated time, from a seed. Packets are lost, reordered and delayed, connections
      reset, disks fail and clocks jump as the seed decides, and a failing run replays
      exactly from its seed.
- [x] Health checking and server reflection, so standard tools (`grpcurl`, Kubernetes gRPC
      probes) work against a giong service.
- [x] Tested against grpc-go in both directions with gRPC's interoperability test cases,
      the message framing fuzzed, and benchmarked against tonic: calls per second, p99
      latency and memory per open stream, for unary and streaming calls.

**Exit criteria:** the interoperability test cases pass against grpc-go as client and as
server, and a benchmark compares unary and streaming calls with tonic.

## 1.2 — macOS (done)

Released as 1.2.0 on 2026-11-22. Shipped: the kqueue driver, what macOS does differently
(`docs/design/portability.md`), keychain trust roots and the system's DNS servers, CI on
Apple Silicon, and `ENOBUFS` handled by waiting. Shutdown signals use a handler and a
pipe rather than `EVFILT_SIGNAL` (the portability document says why). The worker guard
and overload protection move to a later 1.x.

giong builds and runs on macOS, for developing and testing services on a Mac. Production
stays on Linux. Two safeguards for code written in a hurry, on every system, ship with it.

- [x] A kqueue driver behind the same operations as io_uring and epoll: readiness like
      epoll, file operations on the pool of blocking threads, `EVFILT_USER` for wake-ups
      from other threads and `EVFILT_SIGNAL` for shutdown signals.
- [x] What macOS does differently: workers share one listening socket where
      `SO_REUSEPORT` does not spread connections, CPU pinning is skipped (macOS has no hard
      affinity), and `sendfile` and `fallocate` use their macOS counterparts.
- [x] Trust roots for `giong-tls` from the keychain, and DNS servers from the system
      configuration. Kernel TLS stays Linux only: `Ktls::Auto` keeps connections in the
      process.
- [x] CI on Apple Silicon: the test suite and the examples.
- [ ] A worker guard: code that blocks a worker (a blocking call, a long loop) is reported
      while it happens, with where it was spawned and its stack, a coded error and the fix
      (`spawn_blocking`, or a yield in the loop).
- [ ] Overload protection: an adaptive limit on requests in flight and load shedding with
      `503` and `Retry-After` before latency climbs, the path that 1.5's memory limit refuses
      work through, so a small server under a spike slows down gracefully instead of
      falling over.

**Exit criteria:** the test suite passes on macOS in CI, and the examples run there
unchanged.

## 1.3 — HTTP/3 (done)

Released as 1.3.0 on 2026-11-22. Shipped behind the `unstable-http3` and
`unstable-udp-offload` features: giong-quic on quinn-proto's engine (the fallback this
milestone planned for), GSO, GRO, ECN and the local address of a datagram, HTTP/3 and QPACK
in the server and the client with `Alt-Svc`, and h3spec in CI. The native QUIC engine,
automatic HTTPS, browsers in CI and the bare-metal benchmark move to a later 1.x.

HTTP/3 (RFC 9114) over QUIC (RFC 9000) in `giong-http`'s server and client, for the same
handlers.

- [ ] QUIC of its own, in a new crate, `giong-quic`, built for one core per connection:
      packets encrypted in place in the pool buffer, a whole flight sent with one
      segmentation-offload submission, connection IDs steered to their core, and no lock
      shared between workers. Crypto stays with rustls. Held to the QUIC interop runner
      against quinn, quiche, ngtcp2 and msquic, and fuzzed against quinn-proto. If it does
      not pass the core interop cases at a checkpoint about four weeks in, this release
      ships on quinn-proto behind the same API and the native engine follows in a later
      1.x. Then TLS 1.3 through rustls's
      QUIC support (RFC 9001), loss recovery and congestion control (RFC 9002), connection
      migration, and 0-RTT off by default.
- [ ] UDP for QUIC in `giong::net`: segmentation offload on send (GSO) and receive (GRO),
      ECN, the local address of each datagram, path MTU probing, and many datagrams per
      system call. A connection's packets reach the worker that owns it by connection ID,
      not by the sender's address, which changes when a client moves between networks.
- [ ] HTTP/3 and QPACK (RFC 9204) in the server and the client, advertised with `Alt-Svc`,
      with the same limits and coded errors as HTTP/2. Frames, QPACK and packets fuzzed.
- [ ] Automatic HTTPS: certificates from Let's Encrypt or any ACME server (RFC 8555,
      TLS-ALPN-01), renewed in the background, and one call that serves HTTP/1.1, HTTP/2
      and HTTP/3 for a domain. Tested against Pebble, Let's Encrypt's test server.
- [ ] Tested with h3spec, the QUIC interop runner and browsers (Chromium and Firefox in
      CI), with packet loss and reordering injected; benchmarked on bare metal against
      HTTP/2 on giong and against quinn with h3 on Tokio.

**Exit criteria:** browsers load pages from a giong server over HTTP/3, the interop
runner's core test cases pass, and the bare-metal benchmark is published with its method.

## 1.4 — Windows (done)

Released as 1.4.0. Shipped: the IOCP driver for the `giong` crate (tasks, timers, channels,
TCP, UDP, files, DNS from the network adapters, Ctrl+C and the other console events), the
workers of an address sharing one socket, and CI and soaks on Windows Server 2022 and 2025.
The other crates on Windows, trust roots from the certificate store, processor groups,
registered I/O and the published benchmark against Tokio move to later 1.x releases.

giong builds and runs on Windows, for services that have to run there. Linux stays the
platform it is tuned for.

- [x] An IOCP driver. I/O completion ports complete operations the way io_uring does, so
      buffers handed over and handed back fit as they are: `AcceptEx`, `ConnectEx`,
      `WSARecv`, `WSASend`, `TransmitFile` and overlapped file I/O, with `CancelIoEx` and
      each buffer kept until the kernel has let go of it. A zero-byte receive keeps idle
      connections from holding a pool buffer each.
- [ ] What Windows does differently: no Unix domain sockets or file modes at first, workers
      share one listening socket instead of `SO_REUSEPORT`, Ctrl+C, Ctrl+Break and closing
      the console stop the runtime, and CPU pinning goes through processor groups.
- [ ] Trust roots for `giong-tls` from the Windows certificate store, and DNS servers from
      the network adapters.
- [ ] CI on Windows: the test suite and the examples. Speed measured against Tokio on the
      same Windows machine, and published as it comes out. Tokio emulates readiness on top
      of I/O completion ports; a driver that uses completions directly does not have to.
- [ ] Registered I/O (RIO), Windows' registered-buffer interface, closest to io_uring,
      where a benchmark shows it beats plain completion ports.
- [ ] `giong-mcp` answers "why is this slow, why is it stuck" on every system: the slowest
      places tasks are spawned from, blocked workers, pools waiting, shed requests, in one
      report an agent can act on.

**Exit criteria:** the test suite passes on Windows in CI, and a benchmark against Tokio on
Windows is published with its method.

## 1.5 — Lean under pressure, and a developer kit

Hardware, memory above all, is expensive, so a service should carry heavy load on a small
machine and never fall over for lack of memory. giong already spends 5 to 10 times less
memory per idle connection than Tokio and Go (see the README); what no Rust runtime offers
is a memory limit it keeps whatever clients do. Like the releases above, this one breaks nothing:
everything is opt-in, and code written for 1.0 behaves as before. It stays a runtime
feature: the limit and its ledger live in `giong`, and the protocol crates charge what they
hold to it.

- [ ] A memory limit for the whole process (`Builder::memory_limit`, or read from the
      container's cgroup), kept by one ledger per worker with no lock: buffers, request
      bodies, HTTP/2 and TLS state, queued messages, tasks, and the kernel's socket memory,
      which a container's limit counts too. As memory runs short giong gives back what it
      can spare first, then slows clients down by not reading from them, then refuses new
      work (`503` with `Retry-After`, `REFUSED_STREAM`), and only last cuts the one request
      holding the most memory, with a coded error naming it. It never aborts.
- [ ] Memory charged to the request that holds it: a declared estimate per request, and an
      optional counting global allocator that wraps the system's or another (jemalloc,
      mimalloc), so the limit covers the application's allocations too. giong-mcp shows
      what holds memory, by connection and request.
- [ ] Accounts per tenant in the ledger, under a key the service chooses, so one tenant
      over its quota of memory or requests in flight is refused while the others are
      served. The runtime keeps the accounts; which key to use (a client address, a header,
      a TLS identity) is decided in `giong-web`, with the rate limiting planned for 1.x.
- [ ] Idle connections as small as the kernel's share of them or smaller, where it is not
      done before: an idle HTTPS connection under 4.5 KiB (12.5 KiB in 0.7, 6.1 in 1.5.2;
      nginx with OpenSSL 15), an idle HTTP/2 connection under 3 KiB (4.5), and HTTP/2 requests with
      at most 4 heap allocations each (22).
- [ ] Proven under attack: slow uploads, slow heads, slow readers, floods of HTTP/2 streams
      and of new connections, handlers that allocate more than they declared, alone and
      mixed, in containers of 64, 128 and 256 MiB, in CI, under the simulation driver and on
      bare metal.

Before any of this becomes public API, a prototype on a branch answers what the API should
be, and the two-tenant test below decides it. The prototype needs nothing from the other
1.x releases and starts once 1.0.0 is out. The names of the budget types, the shape of the
ledger and how quotas are shared are chosen from its results, not before.

- [ ] **Prototype: two resources counted exactly.** Bytes of buffers, and I/O operations
      waiting for their completion. The charge follows the resource when it moves from a
      request to a child task or to the driver (a dropped future hands its buffers to the
      driver, which keeps them until the completion, as STABILITY.md's *Cancellation*
      says), and is given back at that completion, not at the drop. It grows out of the
      per-worker budget `max_buffered_request_bytes` already keeps, rather than a second
      count beside it. Tests catch a share given back too early, given back twice, and
      never given back; the driver's last-resort path that leaks memory it cannot account
      for counts as never given back.
- [ ] **Prototype: tenants kept apart in the request queue and in the upstream connection
      pool.** It says how much of B's share A may borrow while B is idle, and how B is
      served when it comes back while A's borrowed slots are still busy. In-flight I/O
      cannot be taken back, so B's wait on return is bounded by how long a borrowed slot
      can stay busy (its deadline, then its completion); the prototype measures that bound
      and limits borrowing to what keeps it within B's deadline.
- [ ] **Prototype: what happened to each request.** Four times for every request: queued,
      given a slot, past its deadline, and its I/O really finished; and while it waits,
      which tenant holds the slots and how many of those requests are already past their
      deadline. The first version answers one question: is B slow because its upstream
      answers slowly, or because it waits for slots that A's expired requests still hold?
      Full record and replay comes later (track R).
- [ ] **Acceptance: two tenants on a small machine.** A gateway with 1 vCPU and 128 MiB,
      the same scenario on io_uring and on epoll. A and B are first measured alone for a
      baseline. Then B keeps sending at the same fixed rate (load sent on a schedule, with
      latency measured from when each request was due, so a slow server cannot slow the
      load down) while A's load rises or A's upstream slows down. The deadline is fixed
      before measuring. Target: while A is overloaded, B's requests completed within the
      deadline per second stay at 90% or more of B alone at the same rate. Reported per
      tenant: p99, p99.9, refused and expired. When callers cancel while I/O still runs,
      the ledger still shows what is held, and after the last completion bytes and slots
      are back at their baseline. With B absent, A's throughput with borrowing on and off;
      after A's upstream recovers, how long B takes to get back to normal. Those two
      numbers are what isolating tenants costs.

### The developer kit: the `giong` command

Rust is excellent for what ships and slow for the hours before: edit, compile, restart,
lose the connection, retry. Every team assembles its own kit, and an agent has to guess
which. giong owns the runtime, the HTTP stack, the database clients, the MCP server and a
simulator, so its kit can do what tools working from outside a process cannot. One
binary, `giong`, one command per job, `--json` on every command and an MCP server of its
own, so an agent drives it as easily as a person. The whole kit ships in 1.5.0.

- [ ] **The first hour:** one-command install on every system; `giong new` from templates
      tested in CI; `giong ai init` (AGENTS.md, the skill, the MCP settings for the
      agent); `giong doctor` for the machine; `giong check --json` with each compiler
      error explained; `giong clean` for old build output; and `giong dev`, which restarts
      the service on every change without dropping a request: the new process takes the
      listening sockets, the old one finishes what it was answering.
- [ ] **The daily loop:** `giong fix` for mechanical fixes, `giong test`, databases started
      and migrated (`giong db`), configuration from the environment, a local dashboard
      built on giong-mcp, and an OpenAPI description of the routes and their errors.
- [ ] **Ship and tune:** `giong top` for a running service, `giong bench` on the benchmark
      lab, real requests recorded as tests, and a container image built without Docker.

HTTP/3 is hardened in the same release, on its way to dropping `unstable`:

- [ ] QUIC under attack (handshake floods, gappy and out-of-order stream data, many tiny
      frames, connection ID churn, 0-RTT replay) with memory shown to stay bounded; packet
      input fuzzed; h3spec at 86 of 86; the interop runner's core cases and browsers
      against the example server.

**Exit criteria:** no out-of-memory kill in any case of the attack matrix, well-behaved
clients mixed into it keep at least 90% of their requests per second, the limit costs at
most 1% more instructions per request, and the cheapest-machine benchmark of 0.9 is
published again with the limit on, against Tokio, Go and nginx, and the two-tenant test
passes on both drivers. For the kit: `giong new` to a running service in one command on
Linux and macOS, templates built and tested in CI, and `giong dev` restarting under load
with no failed request.

## 1.5.x — Debugged and proven

No new features. After 1.5.0, a long pass of debugging and improving every platform, with
fixes shipped as 1.5.x patch releases; then every claim measured again on real machines.
Nothing for 1.6 starts before this milestone is done.

- [ ] Rounds of review, each code reading and a live attack (fuzzing, fault injection,
      soaks, hostile clients), on Linux, macOS and Windows, repeated until a whole round
      finds no blocker. Windows and macOS get the most rounds: they are the newest.
- [ ] What 1.1 to 1.5 left open that is a fix, not a feature, shipped as a 1.5.x patch.
      Among them, containers: workers sharing a port under gVisor, a coded error when the
      container's process limit stops a worker from starting, and io_uring that a sandbox
      refuses at setup falling back to epoll.
- [ ] Then every published number measured again on rented bare metal, with method and
      raw data in the repository: the benchmark lab from 1 to 16 cores, the adaptive spin
      governor (#186) that a cloud VM could not judge, and HTTP/3 between two machines over
      a real network card, against HTTP/2 on giong and against quinn with h3 on Tokio.
      Losses are published with the wins.

**Exit criteria:** a whole review round on the three platforms finds no blocker, and the
bare-metal results are in the repository and on giong.run.

## 1.6 — HTTP/3, stable

Proposed: what 1.6 and 1.7 hold is settled once 1.5.x is done, and no 1.6 work starts
before then. HTTP/3 leaves `unstable`, and the first wave of the runtime core lands where
its proposal is accepted.

- [ ] A final review of giong-quic's API and of giong-http's HTTP/3 surface (one way per
      task, coded errors), then `cargo-semver-checks` against 1.3.0.
- [ ] `unstable-http3` becomes a stable `http3` feature, with the old name kept as an
      alias, and `unstable-udp-offload` likewise once it is ready; whether giong-quic keeps
      its own minimum Rust version (1.85) is decided here.
- [ ] Structured concurrency from [Runtime core](#runtime-core-track-r-after-10): scopes
      whose child tasks never outlive them, task groups, and a deadline per scope that the
      kernel enforces on every operation inside it.

**Exit criteria:** HTTP/3 needs no unstable feature, and the semver checks pass.

## 1.7 — Work inside the kernel

Proposed. The io_uring features that let the kernel do the work a process would, from
[Runtime core](#runtime-core-track-r-after-10), each only with a benchmark that shows what
it buys and no regression anywhere else.

- [ ] `splice` between sockets, so a proxy moves bytes without copying them through the
      process.
- [ ] Wake-ups and descriptors sent between workers' rings (`MSG_RING`) instead of through
      an eventfd.
- [ ] Receive bundles (several buffers per completion) and registered buffers.

**Exit criteria:** each item ships with its benchmark and raw data; none costs an existing
case more than 1%.

## Runtime core (track R, after 1.0)

What a general runtime still lacks, from a [gap analysis of the core](docs/reviews/2026-11-runtime-gaps.md)
against Tokio, Seastar, glommio, monoio, Go and Erlang. This track runs beside the protocol
and platform tracks; each item ships in the first 1.x release whose gates it passes, breaks
nothing, and lands only with its tests under the simulation driver, a coded error and page
for every new failure, and a benchmark with raw data for any claim about speed or memory.
Like the rest of 1.x, it stays on branches until 1.0.0.

First wave, needing nothing else:

- [ ] Blocking and compute work: `spawn_blocking` on a bounded pool, and a compute pool
      sized from the same CPU budget as the workers (the container's quota), with a bounded
      queue, so a blocking call or a CPU-heavy job never stalls a worker.
- [ ] Processes and signals: child processes waited for through the ring (`waitid`, or a
      pidfd), their standard input and output as streams, the process's own stdin and
      stdout, pipes, any signal as a stream, Unix datagram sockets, passing descriptors
      between processes, and listeners taken from systemd.
- [ ] Structured concurrency: scopes whose child tasks never outlive them, task groups,
      values local to a task, and a deadline per scope that the kernel enforces on every
      operation inside it (a linked timeout in io_uring, the timer wheel on epoll), with
      the reason for a cancellation as a coded error.
- [ ] Work done inside the kernel: `splice` between sockets, wake-ups and descriptors sent
      between workers' rings (`MSG_RING`), registered buffers, receive bundles.

Second wave, after the first:

- [ ] Scheduling groups: shares of CPU and of disk I/O per class of work (requests,
      background jobs, tenants), so background work does not raise request latency.
      Needs the scopes.
- [ ] Restarts that drop nothing: the new process takes the listeners from the old one,
      which drains; later, idle connections are handed over too, kernel TLS ones included.
      Needs descriptor passing.
- [ ] Record and replay: a bounded recording of everything nondeterministic a worker sees
      (completions, timers, wake-ups from other threads, received bytes), which the
      simulation driver replays exactly, so a failure in production becomes a test an
      agent can run. Needs the simulation driver.

Third wave, measured on bare metal:

- [ ] Workers matched to the network card's receive queues, so a connection runs on the
      core its packets arrive on.
- [ ] Elastic cores: a quiet service gathers its connections onto fewer workers and lets
      the rest sleep, and spreads out again under load, so several services share a small
      machine.

The cgroup's memory limit and its pressure signal (PSI) belong to the memory limit of 1.5,
which reads them and tightens before the kernel's out-of-memory killer acts.

**Exit criteria:** per item, as above. The track is done when every item has shipped in a
1.x release.
