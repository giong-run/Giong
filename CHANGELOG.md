# Changelog

All notable changes to giong are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is `0.x`, a minor release (`0.1` → `0.2`) may contain breaking changes.

## [Unreleased]

### Added

- **giong-compat**, a new crate: `Compat` gives giong's `TcpStream`, `UnixStream` and (with
  the `tls` feature) giong-tls's `TlsStream` the `AsyncRead`, `AsyncWrite` and
  `AsyncBufRead` traits of `tokio::io` and of `futures_io`, so that crates generic over
  their transport run on giong with no Tokio runtime: tested with tokio-rustls,
  futures-rustls, tokio-tungstenite, h2 (also over giong-tls with ALPN), hyper's HTTP/1
  connection API (against giong-http both ways) and tokio-util's codecs, on io_uring and
  epoll. Writes are buffered like `BufWriter` (64 KiB by default) and sent on `flush`, when
  the buffer is full or when a read waits; a partly read chunk never holds the worker's
  receive pool; `TCP_NODELAY` is turned on. It costs a copy each way (none for reads
  through `AsyncBufRead`), measured by `bench/compat.sh`. Errors `GIONG-E1401` (an earlier
  buffered write failed), `E1402` (write after shutdown) and `E1403` (invalid setting).
- **giong**: `TcpStream::poll_recv` and `UnixStream::poll_recv`, the poll form of `recv`,
  for futures written by hand and for adapters such as giong-compat.

### Fixed

- **giong**: a `UdpSocket` bound to port 0 could get a port that another UDP socket of the
  same user already held, in this process or another, and the two sockets then split
  each other's datagrams by a hash of the sender's address: answers went to the wrong
  socket and the one waiting for them never saw them. With `SO_REUSEADDR` or
  `SO_REUSEPORT` set, Linux picks a port for port 0 without avoiding the sockets that hold
  one with the same options; of 1,000 sockets bound to `127.0.0.1:0` at once, about 15
  pairs shared a port. `UdpSocket::bind` now sets neither option for port 0, so such a
  socket has its port to itself (binding that port again fails with `AddrInUse`);
  sockets bound to a port given explicitly share it between workers as before. This was
  the rare UDP failure in 0.4.0's CI. TCP listeners were not affected.
- **giong**, ThreadSanitizer builds only (`--cfg giong_tsan`): a socket or file closed
  with `close(2)` could still have operations queued in the ring, and a socket that got
  the same descriptor number next could have its data taken by them. Queued operations
  now go to the kernel before the close.
- **Tests**: tests that took a port from a socket closed at once, to connect where nothing
  listens or to share it between workers, now hold it with a socket that is bound but
  does not listen, so a test running alongside cannot take it in between. The
  load-balancing test judges the hand-over rule with exact load figures instead of timing
  (no worker more than two connections ahead, every run); the burst of connections is
  still run, and its spread printed, not judged.

## [0.4.0] - 2026-11-13

Foundations for services, and a look inside them. giong now runs where io_uring is
blocked or missing (Docker and Kubernetes with their default seccomp profiles, gVisor,
kernels before 6.0) on an epoll driver it falls back to by itself, about as fast as Tokio
there, and it says when and why it does. On Ubuntu 24.04's own 6.8 kernels, whose
backported bug rejects every io_uring buffer ring, it now runs on io_uring at full speed.
`giong-http` streams bodies both ways, `giong-web` gains `tracing`, and an example
REST API built only on giong's crates (PostgreSQL, Redis, HTTPS) serves 1.5 to 2.1 times
as many requests per second as the same API on axum. The new `giong-mcp` lets an AI agent
look inside a running service and names the task that blocks a worker. An idle
connection now costs 1.9 KiB instead of 10.9 (axum 20.4, Go 18.9), and new work is
balanced across workers when it is placed. Two changes in `giong-http` break code that
used it: `Body` is `#[non_exhaustive]`, and a closure passed to `Server::serve` needs its
argument's type written.

### Added

- **giong**: `giong::driver_choice()` (on a worker) and `Handle::driver_choice(worker)`
  (from any thread) say which driver a worker runs on, whether `Builder::driver`,
  `GIONG_DRIVER` or giong chose it, why io_uring is not used when giong fell back to epoll
  (the full `GIONG-E…` error), and which kernel bugs giong works around. Falling back to
  epoll by itself is also said once per process on stderr, with the reason; naming a
  driver chooses without it. `giong-mcp`'s `overview` shows the choice for each worker
  and warns when workers fell back to epoll.
- **Verification**: CI's `test`, `fallback` and `tsan` jobs fail if giong would not choose
  io_uring by itself on the runner (`GIONG_TEST_EXPECT_DRIVER=io_uring`), instead of quietly
  testing epoll only, and the whole suite also runs on Ubuntu 24.04's own kernel
  (6.8.0-147) in QEMU: `scripts/kernels.sh ubuntu-6.8.0-147` boots any kernel Ubuntu ships.
  Every QEMU run checks the driver giong chose and the kernel bugs it worked around.
- **An epoll driver**, so that giong runs where io_uring is blocked or missing: in a
  Docker container with the default seccomp profile, in Kubernetes with `RuntimeDefault`,
  under gVisor, with the `kernel.io_uring_disabled` sysctl, and on kernels older than 6.0
  (tested on 5.15). A worker falls back to it by itself when io_uring cannot be set up;
  `Builder::driver(DriverKind::IoUring | DriverKind::Epoll)` or `GIONG_DRIVER=io_uring|epoll`
  choose explicitly (asking for io_uring where it is blocked fails with `GIONG-E0001`, as
  before), and `giong::driver_kind()` tells which driver a worker got. Every API behaves
  the same on both; on epoll, file operations run on a pool of blocking threads, except on
  pipes and sockets, and a file is closed only once the threads are done with it. On one
  core, epoll does what raw Tokio and a hand-written epoll server do (158k requests per
  second on a small response at 50 connections, 2.02 system calls per request), while
  io_uring keeps its lead where batching counts: 55.7k against 32.7k requests per second
  on an 8 KB response. The README has the full comparison.
- **Verification**: `scripts/check.sh fallback` also runs the suite on epoll;
  `scripts/check.sh seccomp` runs it with io_uring blocked the way Docker blocks it
  (`scripts/no-io-uring.py`), and CI runs it inside a Docker container with the default
  profile and on a real 5.15 kernel. ThreadSanitizer runs on both drivers.

- **`giong-mcp`**, a new crate: an MCP server that lets an AI agent look inside a running
  service. Started with `GIONG_MCP=127.0.0.1:7777` (or `Config::tcp`/`Config::unix` in
  code), it listens only on loopback or a Unix socket (`GIONG-E1301` otherwise), refuses
  requests whose `Host` or `Origin` is not local, and takes an optional bearer token. Five
  read-only tools: `overview` (process, workers, warnings), `workers`, `tasks` (live tasks
  by the line that spawned them), `sources` (connection pools) and `profile` (where the
  workers spend their time over a window). It runs on a thread of its own, so it answers
  when every worker is stuck, and names the task a blocked worker is in.
- **giong**: `giong::observe`, what giong-mcp reads. `Handle::pulse(worker)` returns a
  worker's counters (event-loop turns, polls, sleeps, idle time, spawned tasks) and the
  task it is polling, from any thread, without disturbing it. `spawn`,
  `Handle::spawn_on` and `Handle::spawn_balanced` record the line that called them, and
  `observe::snapshot()` groups a worker's live tasks by it, with its timers, I/O in
  flight, receive buffers and registered `Source`s. `Handle::set_profiling` and
  `observe::take_profile()` time task polls by spawn location. The counters cost about
  nine instructions per task poll (callgrind, tasks that only yield); profiling, while
  off, one load per batch of tasks.
- **giong-postgres**, **giong-mysql**, **giong-redis**: pools register themselves as
  `giong::observe` sources (server and database, never credentials), so giong-mcp shows
  their connections, waiters and errors.

- **Streaming bodies in `giong-http`'s server**, both ways, with backpressure.
  - Request bodies read as they arrive: a handler whose `Handler::stream_body(&req)`
    returns `Some(limit)` is called once the head is in, and reads the body with
    `req.body_stream()` (`BodyStream::chunk`, `read_to_end`), one piece in flight at a
    time. Nothing is read until the handler asks, so it can refuse an upload before the
    client sends it (`100 Continue` only then). `Content-Length` and chunked bodies are
    both streamed; a body announced past the limit gets `413` without calling the
    handler, one that grows past it fails with `GIONG-E1111`; a client that closes or
    stalls (each piece gets `request_timeout`, the whole body no deadline) gives
    `GIONG-E1112`, broken chunked framing `GIONG-E1113`. A handler that answers before
    the end of the body gets its response sent, then the connection closes.
  - Response bodies written as they are produced: `Body::channel()` gives a
    `BodySender` and a `Body` for the response. The head goes out when the handler
    returns, each piece as soon as it is sent, with `Transfer-Encoding: chunked` (to an
    HTTP/1.0 client without a length, then the connection closes). `send` waits until
    the previous piece is written; it fails with `GIONG-E1114` once the client is gone
    or the response has no body (`HEAD`, 204, 304). `BodySender::abort` ends the body
    without its last chunk, so the client sees it is incomplete. Large pieces are written
    without a copy, as large bodies are.
  - The connection fuzz target streams request and response bodies too.
- **`giong-web`**: streaming and `tracing`.
  - `Router::stream_bodies(max)`: the routes of that router (wherever it is nested) read
    their body as it arrives, with `Req::body_stream()`, up to `max` bytes. `?` on a
    body error answers `413` (`GIONG-E1111`) or `400` (`GIONG-E1112`, `E1113`);
    `Req::json` on such a route is a `500` whose logged cause says to use the stream.
    `Timeout` leaves these requests alone, `Compression` leaves streamed responses
    alone, and `Logger` writes `"bytes":null` for them.
  - `middleware::Trace`, behind the new `tracing` feature: each request runs in a
    `request` span (target `giong_web`) with OpenTelemetry's field names
    (`http.request.method`, `url.path`, `http.route`, `http.response.status_code`,
    `client.address`) plus `request_id` and `worker`, so events the handler records carry
    them; the span ends with one event, at `ERROR` with the code and cause for a `5xx`.
  - 4 new error codes, `GIONG-E1111` to `E1114`; 87 in all.
- **`examples/rest-api`**, a complete REST API on giong's own crates: users in
  PostgreSQL (`giong-postgres`), read through a Redis cache (`giong-redis`), served over
  HTTPS (`giong-tls`) by `giong-web`, with create, read, update, delete, keyset pages and
  an NDJSON export streamed with `Body::channel`. Each worker has its own pools and
  listener. Its test starts a throwaway PostgreSQL and Redis and goes through every route,
  including reads with the cache down. `bench/rest-api.sh` measures it against the same
  API on axum 0.8 (`bench/rest-api-axum`), one worker each over HTTPS: 69.9k against
  47.2k requests per second on cached reads, 35.9k against 19.5k on pages from
  PostgreSQL, 54.1k against 25.8k on inserts (medians of 5 rounds; method and ranges in
  the example's README).

- **Load balancing across workers**, without moving a task once it has started (so tasks
  still need not be `Send`):
  - `Handle::spawn_balanced(f)`: `f` waits in a queue shared by the workers, and the
    first worker with nothing else to run takes it; busy workers also take one now and
    then, so no job waits for ever. 64 jobs of uneven CPU cost started from one worker
    finish in 5.7 ms on 4 workers (median of 15 rounds), against 6.9 ms placed
    round-robin and 16.8 ms on the worker that started them; the slowest round took 6.5,
    10.4 and 19.7 ms (`bench/spawn-balanced`).
  - `TcpListener::bind_balanced(addr)`: listeners of the same runtime on the same address
    form a group, and a worker that looks clearly busier than another member hands each
    new connection it accepts to that member. `SO_REUSEPORT` alone spreads connections by
    a hash of their addresses, so with 6 connections on 3 workers running a CPU-heavy
    route the result depends on where the hash puts them: over 10 runs plain listeners
    gave 37,000 to 54,000 requests per second (median 51,600, p99 448 µs), balanced ones
    47,000 to 59,000 (median 53,000, p99 408 µs) (`bench/balance.sh`).
  - `Handle::least_loaded_worker()`, from hints each worker publishes between batches of
    tasks (live tasks, runnable tasks, asleep or not), plus work sent to a worker that it
    has not taken in yet. Publishing them costs about 0.1% of the instructions per request
    of a small HTTP response.

- **`giong-web`**, a new crate: routing, middleware and JSON for HTTP services on
  giong-http.
  - A handler is an `async fn(Req<S>) -> Result<Response, Error>`, and `?` works on any
    error. `Router::new().get("/users/{id}", user).nest("/teams/{team}", members)
    .layer(m).build(state)` builds an `App` per worker, with no locks and nothing `Send`.
    Patterns have parameters (`{id}`) and a catch-all (`{*path}`); a literal segment wins
    over a parameter, a parameter over a catch-all, with backtracking. `HEAD` is answered
    by the `GET` route, `OPTIONS` and `405` with `Allow` are answered for every path,
    and a `404` for `/users/` names `/users` when that exists.
  - `req.param::<u64>("id")?` and `req.query_param::<u32>("page")?` parse into any
    `FromStr` type: a value that does not parse is a `400` (`GIONG-E1205`) naming the
    parameter and the expected type, and asking for a parameter the route lacks is a
    `500` (`GIONG-E1206`) whose cause names the route. With the `serde` feature,
    `req.json::<T>()` (`415`, `400` or `422`: `GIONG-E1208`, `E1207`), `req.query::<T>()`
    and `json(status, &value)`.
  - Every error answer is an RFC 9457 problem+json body with a stable code (`{"status":
    404,"code":"GIONG-E1203","title":"Not Found","detail":"no route for GET /x"}`), and
    the request id when there is one. `Error::bad_request(code, detail)` and its
    siblings make one with your own code; an error that is not one becomes a `500`
    (`GIONG-E1210`) whose cause is logged and never sent, or a `503` with `Retry-After`
    (`GIONG-E1211`) when it is a giong error for a database or service that cannot be
    reached. A 5xx no `Logger` reported goes to stderr. Two routes for the same requests,
    or an invalid pattern, panic at startup with a code and the line that added them.
  - Middleware is a trait with one `async fn call(&self, req, next)`; they run in the
    order added, and those of a nested router only for its routes. Included:
    `RequestId` (`X-Request-Id`, incoming ids trusted only if asked), `Logger` (one JSON
    line per request with its route, status, duration, request id and, for errors, the
    code and cause; written by a background thread, never on the worker), `Cors`
    (nothing allowed until named; a preflight that is refused says why, `GIONG-E1212`),
    `Compression` (gzip at level 1 for bodies from 1 KiB, in 64 KiB slices that yield to
    other tasks) and `Timeout` (`503`, `GIONG-E1209`). Settings that cannot work, such as
    any origin with credentials, panic with `GIONG-E1213`.
  - `App::handle(Request)` answers without a socket, so routes and middleware are tested
    with plain unit tests (`Request::from_bytes`).
  - On one core against axum 0.8 with the same serde handlers (`bench/web.sh`, median of
    5 rounds): 152.5k requests per second on `/json` against 118.3k, 28.0k on a list of
    100 users against 15.2k, 140.5k on `/users/{id}` against 107.1k; with request ids,
    gzip and a timeout, 126.2k against 65.8k on `/json` and 12.0k against 5.2k on the
    gzipped list (tower-http for axum). The router, parameters and serde cost about 5%
    against giong-http's bare handler.
  - Two fuzz targets (`giong-web/fuzz`, 17 in all): the route table against a slow
    reference matcher, and any bytes as a request to an app with every middleware, whose
    answer must be well formed (status and headers, problem+json with a code for every
    error, gzip that decodes to what the handler wrote, one JSON log line that agrees
    with the response, the request id sent back).
- **`giong-http`**: `Handler`, the trait the server calls; every `Fn(Request) -> impl
  Future<Output = Response>` is one, and a type can implement it to borrow from itself
  while it answers (giong-web's `App` does). `Response` gains `headers()`,
  `get_header`, `remove_header`, `body_bytes`, `take_body` and typed extensions
  (`with_extension`, `extension`); `Request` gains `from_bytes`, to make one in a test,
  and `with_peer_addr`. `reason_phrase(status)`.
- `giong::try_worker_id()`: the index of the worker running the caller, or `None` off
  the runtime. 13 new error codes, `GIONG-E1201` to `E1213`, for giong-web; 81 in all.

- **`giong-http`**: what the client lacked for production traffic.
  - HTTP proxies: `ClientBuilder::proxy` with `Proxy::all`, `Proxy::http` or
    `Proxy::https`, or `Proxy::from_env` for `HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY` and
    `NO_PROXY`, read the way curl reads them and only when asked. `NO_PROXY` takes host
    names (and every name under them), addresses and ranges (`10.0.0.0/8`, `fd00::/8`),
    ports and `*`. `http` URLs go to the proxy in absolute form; `https` goes through a
    `CONNECT` tunnel, so the proxy sees only the host name. Credentials in the proxy's
    URL go to the proxy only, never through the tunnel, and never appear in `Debug`
    output or errors. Connections are pooled per route, so direct and proxied ones never
    mix. Plain `http://` proxies only: no SOCKS and no TLS to the proxy itself yet.
  - gzip and deflate bodies: responses are asked for with `Accept-Encoding: gzip,
    deflate` and decompressed as they arrive. The size limit applies to what comes out,
    so a small "zip bomb" fails at the limit instead of filling memory. The gzip checksum
    and length are checked, multi-member gzip and deflate with or without its zlib
    wrapper are read, and a body cut short is an error, never a shorter body.
    `ClientBuilder::decompress(false)` turns it off, a request that sets its own
    `Accept-Encoding` gets the bytes as sent, and `br` and `zstd` bodies arrive
    untouched. New dependency: `miniz_oxide` (pure Rust, the inflater flate2 uses), in
    the `client` feature only.
  - Streamed bodies: `RequestBuilder::send_streaming` returns a `StreamingResponse` once
    the head has arrived, and its `chunk()` gives the body piece by piece (64 KiB at
    most, decompressed), whatever its size. The request timeout covers everything up to
    the head, then each piece. A body read to its end gives its connection back to the
    pool; one dropped part way closes it. As with `send`, an HTTPS body that ends with
    the connection needs the TLS `close_notify`.
  - `ErrorKind::Proxy` (`GIONG-E1110`) for a proxy that refuses a request or a tunnel: a
    407 says how to give credentials, a 403 suggests `no_proxy`, and 502 and 504 are
    transient. A proxy that cannot be reached is `Connect` (`GIONG-E1102`), and its
    message names the proxy. 68 error codes in all.
- **Verification**: coverage-guided fuzzing of every parser that reads bytes from the
  network, with 12 new cargo-fuzz targets (15 in all): the client's response heads, URLs
  and gzip/deflate decoder in `giong-http`; the PostgreSQL messages, SCRAM exchange and
  URLs (`giong-postgres/fuzz/`); the MySQL packets, framing, rows and URLs
  (`giong-mysql/fuzz/`); RESP3 replies and URLs (`giong-redis/fuzz/`). CI fuzzes each
  target for 10 minutes every night, keeping the corpus, and for 60 s on pull requests
  that touch these crates. The randomised tests run the same checks under `cargo test`.
- **Verification**: a `servers` CI job runs the database crates against PostgreSQL 14, 15
  and 17, MySQL 8.4 and 9.1, Redis 6.2 and 7.4 and Valkey 8.0, besides the PostgreSQL 16,
  MySQL 8.0 and Redis 7.0 of the other jobs.
- **`bench/memory.sh`**: memory per connection, for 10,000 connections that are open,
  idle after a request, or waiting for their handler, and the peak under load, against
  Axum, raw Tokio and Go's `net/http` at the same thread count. `bench/hold` holds the
  connections; the README has the results.

### Changed

- **An idle keep-alive connection to `giong-http` costs 1.9 KiB instead of 10.9 KiB**, and
  one whose request is being handled 7.2 instead of 15.0 (Axum: 20.4 and 19.3, Go's
  `net/http`: 18.9 and 18.3; 10,000 connections, `bench/memory.sh`). The connection's
  task was most of it: every wrapper that took a future by value kept two copies of it
  (the task around a spawned future, `time::timeout`, `until_shutdown`, the connection
  limit, `Conn::run`), so a 2.3 KB connection future took a 9.2 KB task. And the
  handler's future lived inside the connection's, so every idle connection was as large
  as the largest future the handler can make: one that held 8 KiB across an `.await`
  made each idle connection cost 43 KiB, 430 MB for 10,000. Now the handler's future is
  on the heap only while a request is handled, in blocks the worker reuses. With 20
  workers and 5,000 busy connections the server peaks 50 MiB above its idle memory
  instead of 120 (raw Tokio: 64 MiB, Axum: 124 MiB, Go: 184 MiB; 14 MiB on epoll).
  Throughput is unchanged within noise (7 interleaved rounds of `/json` at 100 and 1,000
  connections); callgrind counts 2% more instructions per request in user space.
- **giong**: `time::timeout` returns a `time::Timeout<F>` and `until_shutdown` a
  `giong::UntilShutdown<F>`, each only as large as the future it wraps plus a timer or a
  shutdown signal, instead of an `async fn`'s future twice that size. Awaiting them is
  unchanged. A spawned task is as large as its future, no longer twice.
- **giong**: a worker lends the kernel 16 receive buffers at first, and twice as many each
  time a turn of the event loop takes most of them, instead of all 512 at once; free
  buffers are reused most recent first, on both drivers. The kernel takes the buffers of
  its ring in turn, so with all of them lent every buffer was touched sooner or later
  and the whole pool became resident, 2 to 4 MiB per worker however light the traffic:
  with 20 workers and 100 busy connections, `giong-http`'s memory rose by 40 MiB, now by
  1 MiB.
- **`giong-http`**: a connection hands its read buffer back before its responses are
  sent rather than after, so on io_uring, where a buffer stays with the kernel until the
  send completes, the connections of one turn share fewer buffers. The buffers a worker
  keeps for connections to reuse are bounded at 4 MiB, and one that grew past 16 KiB is
  freed rather than kept. Before, up to 1,024 of
  up to 256 KiB each could stay, and a large one could be handed to a small request and
  held for as long as its handler ran.
- **`giong-http`**: `Server::serve_connection` returns its future instead of being an
  `async fn`; the future no longer borrows the `Server`.
- **giong**: a runtime that cannot use io_uring now starts on epoll instead of failing with
  `GIONG-E0001` or `GIONG-E0002`; those errors are raised only when io_uring was asked
  for. An unknown `GIONG_DRIVER` value fails with `GIONG-E0004`. `docs/DEPLOYMENT.md`
  explains when each driver is used.
- **Breaking:** **`giong-http`**: `Body` is `#[non_exhaustive]` and has a new variant,
  `Body::Stream`; a `match` on it needs a `_` arm. `Body::as_bytes` and `len` are empty
  for a stream, and `is_empty` is `false` (`Body::is_stream`, `Response::is_streaming`
  tell). `Server::max_body_size` does not apply to bodies read as a stream.
- **Breaking:** **`giong-http`**: `Server::serve`, `serve_unix` and `serve_tls` take any `Handler`. A
  closure passed to them needs its argument's type written (`|req: Request| async
  move { .. }`): Rust no longer infers it.
- **`giong-http`**: the client asks for compressed responses and decodes them by default
  (see Added); a decoded response no longer has `Content-Encoding` and `Content-Length`
  among its headers. A request that sets `Proxy-Authorization` itself is refused
  (`GIONG-E1101`): the client sets it, for the proxy only.
- **`giong-http`**: the client runs 41% fewer instructions per request (18,795 to 11,115
  under callgrind, `json`). Each request's future held, unused, the future that opens a
  connection, TLS handshake included, and copied its 8 KB or so at every await; that
  future is now boxed and lives on the heap only while a connection opens. The response
  head is no longer copied out of the buffer it was read into either. On one core,
  requests per second rose by 3 to 16%, and with 64 connections the client sends 1.6 to
  2.0 times as many as reqwest (`bench/http-client.sh`, median of 5 rounds).

### Fixed

- **io_uring on Ubuntu 24.04's own kernels.** Ubuntu's 6.8 kernels from 6.8.0-135 on
  (and the AWS, Azure, GCP and Oracle 6.8 kernels and the 6.8 HWE kernel of 22.04 built
  from them) refuse every correctly formed io_uring buffer ring: a backport of "io_uring/kbuf: use mem_is_zero()" inverted
  the check on its reserved fields, so the kernel rejects zeroed ones and accepts others.
  giong 0.3 would not start there, calling the kernel too old, and with the epoll driver
  alone it would have run on epoll without saying so. It now recognises the bug with a probe that changes nothing and registers its
  ring the way that kernel accepts, so io_uring runs at full speed; on correct kernels
  nothing changes. `giong::driver_choice()` lists it under `kernel_workarounds`
  (`KernelWorkaround::BufferRingReservedCheck`). Found on Ubuntu 24.04 bare-metal
  machines (6.8.0-147); checked on 6.8.0-134 (no bug), -135, -147 and -149.
- A kernel from 6.0 on that refuses buffer rings for another reason is no longer called
  too old: the error is the new `GIONG-E0009` and names the kernel release
  (`GIONG-E0002` stays for kernels older than 6.0).
- Tests: `accept_many_connections` and `accept_backlog_is_bounded_and_no_connection_is_lost`
  failed with "too many open files" under the 1024 soft limit that shells start with;
  they raise the soft limit up to the hard one.
- `giong-http` server: more than two blank lines before a request line start the request
  timeout. A client sending `\r\n` once a minute kept a connection open for ever.
- `giong-http` client: a kept-alive connection that received anything while idle (a `408
  Request Timeout` that some servers send before closing, the rest of a body longer than
  its `Content-Length`) is no longer used. The bytes were read as the response to the next
  request.
- `giong-http` client: a `Host` header set by hand is not sent to another origin after a
  redirect, like `Authorization` and `Cookie`.
- `giong-http` client: idle connections of an origin that is not asked for again are
  closed after the idle timeout, and the origin is forgotten. Before, they stayed open,
  with their entry, for as long as the client lived.
- `giong-postgres` and `giong-mysql`: transactions no longer run on the connections a
  pool shares. `Pool::query`, `query_one`, `query_opt` and `execute` (and `insert` in
  giong-mysql) refuse `BEGIN`, `START TRANSACTION`, `COMMIT`, `ROLLBACK`, `SAVEPOINT` and
  the other transaction statements (MySQL also `XA`, `LOCK` and `UNLOCK`) before sending
  anything, with the new error codes `GIONG-E3011` and `GIONG-E3111`
  (`ErrorKind::Statement`). Before, `pool.execute("BEGIN", &[])` left a connection that
  other tasks share inside a transaction: their statements ran in it and were never
  committed, though they had been told they succeeded. A `Pool::batch_execute` that leaves
  a transaction open (or, in MySQL, autocommit off) now closes its connection, which rolls
  the transaction back, and fails with `ErrorKind::Closed`, as do the statements pipelined
  behind it.
- `giong-postgres` and `giong-mysql`: a `PooledClient` given back while a statement is
  still in flight (its future dropped by a timeout) queues a `ROLLBACK` first. If that
  statement was `BEGIN`, the connection went back to the shared pool inside a
  transaction, because the last answer still said there was none. giong-mysql closes a
  connection given back with autocommit off.
- `giong-postgres` and `giong-mysql`: `Pool::get` waiting for a connection is woken when a
  statement whose future was dropped frees it. It waited out the wait timeout and failed
  with `PoolTimeout`.
- `giong-postgres`: rows without columns (`SELECT FROM t`) are counted; `query` returned
  none.
- `giong-mysql`: a `TIME` with more days than MySQL allows no longer overflows when it is
  read as text.
- `giong-redis`: a `watch` cancelled while waiting for `WATCH`'s reply sends `UNWATCH`.
  Before, the watch stayed on the connection, and once a pool shared it, another task's
  atomic pipeline could be skipped by `EXEC` while reporting success.
- `giong-redis`: an atomic pipeline whose `EXEC` answers nil (a watched key changed,
  outside a `Watch`) is an error, "the transaction did not run", instead of a value.
- `giong-redis`: a push message larger than `Config::max_reply_size` (a client-side
  caching invalidation) is passed over. Before, it was counted as the reply to the oldest
  command, and every later reply went to the wrong command. An attribute count out of
  range at the front of such a message (`|9223372036854775807`, or a negative one) is
  no longer multiplied: it overflowed, a panic in a debug build. Found by the `replies`
  fuzz target.
- `giong-redis`: a task waiting in `Pool::take` is woken when a shared command whose
  future was dropped frees the connection. Before, it slept until the wait timeout and
  could fail with `PoolTimeout` while the connection was free.

### Security

- `giong`: `localhost` and names under `.localhost` resolve to the loopback addresses
  without asking DNS (RFC 6761). Before, they went through the search list first, so a
  wildcard record in a search domain, or a forged answer, could point `localhost` at
  another machine, which `giong-postgres` and `giong-mysql` then trusted as local: no TLS,
  the password sent in the clear, MySQL's RSA key fetched unverified. A hosts file entry
  for such a name is used only if it is a loopback address.
- `giong-http`: chunk extensions are limited to 16 KiB per body, on the server and in the
  client. They counted toward no limit, so a peer could send about 4,000 bytes per byte of
  body, all of it held in memory until the body ended (the flaw of Node.js
  CVE-2024-22019). Such a request is answered with 400; such a response is an error.

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

[Unreleased]: https://github.com/giong-run/Giong/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/giong-run/Giong/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/giong-run/Giong/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/giong-run/Giong/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/giong-run/Giong/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/giong-run/Giong/releases/tag/v0.1.0
