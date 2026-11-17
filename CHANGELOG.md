# Changelog

All notable changes to giong are recorded here.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the
project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the
version is `0.x`, a minor release (`0.1` → `0.2`) may contain breaking changes.

## [Unreleased]

### Security

- **giong-http**: the server refuses new connections while the process is close to running
  out of memory, instead of taking on connections until the kernel's out-of-memory killer
  ends it with every connection it holds. Before each accept it checks what is left (the
  smaller of what the process's cgroup allows, page cache aside, and the machine's
  `MemAvailable`), and below a reserve it answers new connections without TLS with `503`,
  `Retry-After: 1` and `GIONG-E1121`, and closes new TLS connections before their
  handshake; the connections it holds keep being served. The reserve is
  `Server::min_free_memory` (new), by default a tenth of the memory limit, between 16 and
  256 MiB; `0` turns the check off. Found on bare metal in a container of 1 CPU and 512
  MiB, where 0.8.0 was killed at 36,400 idle HTTPS connections (with `max_connections`
  raised); the default limit of 10,000 connections per worker was enough to get it killed
  in smaller containers too: 7,900 idle HTTPS connections in 96 MiB, or 9,100 plain ones in
  32 MiB (cgroup v1, which leaves kernel socket memory out). With the check, the same runs
  refuse at 6,500 and 6,600 connections and stay up. CI runs it in a container of 48 MiB
  with one and four workers on both drivers (`scripts/idle-connections.sh`).

### Added

- **giong-http**: `Server::min_free_memory`, and the error code `GIONG-E1121`
  (`Code::HttpServerLowMemory`) in giong.
- **giong-mysql**, **giong-redis** (**behaviour change**): `Pool::idle_timeout`,
  `Pool::max_lifetime` and `PoolStats::recycled`, with giong-postgres's defaults and
  meaning, so the three pools are configured the same way before the 1.0 API freeze. A
  pooled connection unused for 10 minutes is now closed, and one older than 30 minutes (less
  up to a tenth at random) takes no new statement or command and is closed once the ones on
  it are answered, or when it comes back from `get()` or `take()`; the pool opens another
  when needed. Before, a connection stayed open until the server or the network closed it,
  so a pool kept talking to the old primary after a DNS failover. giong-redis reads no
  clock on the path of a shared command for this: a command marks its connection used, and
  the pool's sweep, which runs every quarter of the shorter of the two settings (at most
  every 30 seconds), turns the mark into a time. `observe` snapshots and giong-mcp report
  `recycled` for the two pools.
- **giong-postgres**, **giong-mysql**: `Error::server_code()`, the name giong-redis already
  used for the code the server sent (`code()` named a different thing in each crate), and
  `Error::is_transient()`, as giong-redis and giong-http's client have: true for a failed
  connect, a closed connection, a pool timeout, and the server errors a retry can cure
  (giong-postgres: class `08`, `40001`, `40P01`, `55P03`, `53300`, `57P01`-`57P03`, `25006`;
  giong-mysql: 1213, 1205, 1040, 1203, 1053, 1792, 1836, 1927, and 1290 for a read-only
  server).
- **giong-postgres**: `Config::database`, the name giong-mysql and giong-redis use. The URL
  parameter is still `dbname`, as libpq spells it.

### Changed

- **giong-http**: the client's `RequestBuilder::header` takes `impl AsRef<str>` for the value,
  as the server's `Response::header` does, so `.header("X-Id", id.to_string())` needs no `&`.
  A call that passed a `&str` or `&String` compiles as before; one that relied on the
  parameter being `&str` to infer a type (`value.as_ref()`, `.into()`) now needs `&str` written
  out. `cargo-semver-checks` reports no break.
- **Docs**: giong-web's `Router::layer` and README say that the first middleware added is
  the outermost, the reverse of axum's `Router::layer`, so code ported from axum lists its
  layers the other way round.
- **Docs**: CONTRIBUTING.md lists every fuzz target (`response_body`, the PostgreSQL and
  MySQL `session` targets and the resolver's two were missing); giong-http's
  `Response::json` says it sends bytes that are already JSON; the API freeze record
  (`docs/reviews/2026-11-api-freeze.md`) gains the items of the reviews it had left out.

- **net**: UDP is faster than Tokio's on both drivers. A UDP echo server on one core (64-byte
  datagrams, veth, `bench/udp-echo.sh`, 5 rounds, `bench/results/udp-echo/`) answers 330,000
  datagrams a second with 16 tasks on one socket on io_uring (266,000 before, Tokio 287,000)
  and 350,000 with one task (265,000 before, Tokio 277,000); on epoll 302,000 in both cases
  (262,000 and 242,000 before). `UdpSocket::send_to` and `send` send within the call when the
  socket has room, instead of through an operation that waited a turn of the event loop
  (io_uring) or allocated a message header (both), and use `sendto(2)` rather than
  `sendmsg(2)`. The epoll driver receives up to 16 datagrams with one `recvmmsg(2)`. An echo
  now costs one system call on either driver where Tokio makes two. Immediate sends count
  against the task's budget, so a task that only sends still gives way to the others.
- **net**: the epoll driver's UDP path is at the kernel's ceiling. The same echo server
  answers 394,000 datagrams a second with 16 tasks on epoll (332,000 before, Tokio 295,000,
  the kernel-only `recvmmsg`/`sendmmsg` server 421,000) at 3.26 µs of server CPU per echo
  (4.21 before, Tokio 4.78, kernel 3.14), and 369,000 with one task
  (`bench/results/udp-echo/vm/epoll-deep/`). Two changes: datagrams that tasks send while
  other tasks of the worker are runnable go to the kernel at the end of that turn of the
  event loop, in one `sendmmsg(2)` per socket, each sender still getting its own result and
  datagrams of one socket keeping their order (both drivers; a lone sender sends at once);
  and on epoll a datagram socket that has received 64 datagrams leaves the epoll set until a
  task has to wait for it again, because in the set every datagram received and every one
  sent and freed wakes epoll's callback on the socket's wait queue.

### Deprecated

- **giong-postgres**, **giong-mysql**: `Error::code()`, renamed `server_code()`, and
  giong-postgres's `Config::dbname`, renamed `database`. Both still work, with a warning
  that names the new method, and go in 1.0.0-rc.1.

### Fixed

- **net**: tasks that share a `UdpSocket` and wait in `recv_from` at the same time are each
  woken for a datagram of their own. On io_uring they shared one wake-up slot: only the
  last task to wait was woken, the others saw their datagrams only when something else
  woke them, and a server of 16 tasks on one socket made one kernel entry per datagram. A
  task that stops waiting (its receive timed out) no longer takes the wake-up of the next
  datagram.
- **giong-postgres**: `Pool::max_lifetime(Duration::MAX)` panicked with an overflow when the
  pool opened its first connection (the random part of the lifetime was computed as a tenth
  of it times up to 999); a lifetime too long for the clock now means none, as
  `wait_timeout(Duration::MAX)` does in giong-redis. Found while adding the same setting to
  the other two pools.
- **giong-http**: an idle HTTP/2 connection took 61% more resident memory in 0.8.0 than in
  0.5.0 (6.78 KiB against 4.21 on bare metal, `bench/h2.sh`, 5,000 connections that made
  one request each). Since 0.8.0 a connection kept the memory for its streams for a second
  after each request, so that requests following one another allocate none of it: when
  thousands of connections open together, all of them held it at once, and the allocator
  kept the pages after it was freed. A connection now keeps it only once a request has
  followed another within that second. Since 0.6.0 the queue of handlers to start also
  reserved room for four requests (672 bytes) on its first use; it now starts with room for
  one, as the slots for streams now do too (they took room for four, 1,408 bytes). Measured on a
  4-vCPU VM (Linux 6.18, `bench/hold`, 5,000 connections, one worker), resident memory per
  connection, 0.5.0 / 0.8.0 / now (raw data in `bench/results/low-memory/`): idle 4.56 / 7.09 / 4.54 KiB, one request in progress
  6.68 / 7.42 / 5.88 KiB, ten requests in progress 19.09 / 21.65 / 19.09 KiB. The `instr`
  job did not catch it: its `idle-h2` case opened connections without making a request.
  It now makes one on each, as `bench/h2.sh` does.
- **giong-http**: the first version of the fix above, merged in #125, gave the queue of
  handlers to start back each time it emptied after growing past four. A client with many
  streams in flight grows and empties it all the time, so every burst allocated it again: on
  bare metal HTTP/2 served 4.6% fewer requests per second (1.519M against 1.593M, `bench/h2.sh`
  json). The queue is now given back after the first burst only, which keeps ten long
  requests opened at once from holding it, and kept from the second on until the connection
  is idle. Instructions per request against 0.8.0 (callgrind, `h2load` with 10 connections,
  4-vCPU VM; raw data in `bench/results/low-memory/`): one stream at a time +1.2% before,
  +0.4% now; ten streams +2.1% / +0.3%; a hundred +4.5% / +1.1%. Memory stays as above. The
  `instr` job sent one request at a time and missed it; its new `h2-streams` case sends ten
  at once on one connection, with a limit of 0.5%.

## [0.8.0] - 2026-11-16

Verified in CI: the checks on the way to 1.0 that need no machines of their own now run on
every change, and none of them found a bug in released code. `cargo-semver-checks` holds
the public API to 0.7.0's. Callgrind counts the instructions and allocations of 16 hot
paths on both drivers, and the memory per idle HTTP/1.1, HTTP/2 and HTTPS connection, and
fails a change that moves any past its limit. Kani proves the small `unsafe` core for every
input within stated bounds. A deterministic simulation runs the epoll driver under lost,
reordered and reset connections, failing disks and jumping clocks, about 210 simulated days
per CI run. Every fuzz target runs for 10 minutes each night, and none has an open crash.
A slow-upload attack runs against a server in a 128 MiB container on every change, behind
the one fix that changes behaviour for running services: the memory budget for requests
being received (under Security). No public API changed; two log outputs did (`Logger`,
`Trace`). This release has no benchmark run on bare metal (that is 0.9); the numbers below
are instruction and allocation counts from `bench/instr`.

### Added

- **CI**: `cargo-semver-checks` checks the public API of every change against the last
  release (0.7.0), so a breaking change fails CI; a release pull request that bumps the
  version may break, as semver allows.
- **CI**: an `instr` job counts with callgrind the instructions and heap allocations of 16
  operations on both drivers (spawn, yield, channels, timeouts, TCP and UDP echo, file
  reads, the HTTP/1.1 server, HTTP/1.1, HTTP/2 and TLS client to server, a giong-web
  route, Redis, PostgreSQL and MySQL queries against in-process fake servers) and the heap
  and resident memory per idle HTTP/1.1, HTTP/2 and HTTPS connection, and fails when one
  moves past its limit from `bench/instr/baseline.csv` (1% for instructions and
  allocations, 2% for heap, 10% for resident memory). The counts repeat within 0.07% on a
  shared runner; raw rounds in `bench/results/instr/`.
- **Testing**: a deterministic simulation (`sim/`, not published) that runs giong's epoll
  driver on a simulated kernel: virtual sockets, epoll, eventfds, clocks, threads and
  disk faults, all chosen by a seed, so a failing run replays exactly with
  `GIONG_SIM_SEED`. It runs the core crate's own tests, HTTP/1, TLS and HTTP/2, Redis,
  PostgreSQL and file scenarios under lost, split, delayed and reset connections, refused
  and dropped connects, reordered datagrams, spurious wake-ups, failing disks and jumping
  clocks, and long runs of months of simulated uptime that check nothing grows per
  request. It needs no change to the crate itself and covers the epoll driver only, not
  io_uring, the kernel or the NIC. `scripts/check.sh sim` runs it; see `sim/README.md`.
- **CI**: Kani proofs of the `unsafe` core, in a new `kani` job and
  `scripts/check.sh kani` (docs/kani.md): the slab (any state up to 5 slots), the receive
  buffer pool against a model of the kernel's buffer rings (any state of 2 buffers, on
  epoll, the whole ring and the packed ring: no buffer both lent to the kernel and held,
  lent twice or lost), the lifetime of an io_uring single-shot operation across cancel,
  completion and a zero-copy notification (every order of events), and `KernelBox`. No bug
  found; each proof caught a planted one. The driver's single-shot lifecycle functions now
  take the slab and the graveyard as arguments and are generic over the entry and leftover
  types, so the proofs run the driver's own code; no behaviour change, and the instruction
  counts are unchanged.

### Security

- **giong-http** (hardening, **behaviour change**): the requests a worker is receiving in
  full now share a memory budget, `Server::max_buffered_request_bytes`. Before, each
  connection was bounded (`max_head_size`, `max_body_size`) and so was the number of
  connections, but not the sum: with the defaults about 11 GiB per worker, and 130 clients
  each sending a 1 MB body one byte short got a one-worker server in a 128 MiB container
  OOM-killed (axum with a `Bytes` route dies at the same point). The budget is 64 MiB per
  worker by default, or a quarter of the process's cgroup memory limit divided among the
  workers if that is less (32 MiB for one worker in 128 MiB), and never less than one
  request at `max_head_size` plus `max_body_size`. Bytes count as they arrive, not as
  announced, from HTTP/1.1 heads and bodies (the first 4 KiB of each request excepted, so
  ordinary requests keep being served) and HTTP/2 bodies, until the request is dropped. A
  request that does not fit is refused without calling its handler: over HTTP/1.1 with `503
  Service Unavailable`, `Retry-After: 1` and `Connection: close`, before its body is read
  when its `Content-Length` already does not fit; over HTTP/2 with `REFUSED_STREAM`, which
  clients retry and which does not count against the client's reset budget. A request larger
  than the whole budget gets `413`. New code `GIONG-E1120` (`Code::HttpServerBusy`), named
  in the `503`'s body and in a line on stderr at most once a minute per worker. What changes
  for a running service: under many large uploads at once, requests past the budget get
  `503` instead of being buffered; streamed bodies (`Handler::stream_body`) are not counted.
  With the fix, 450 such uploads (300 over HTTP/1.1, 150 over HTTP/2) leave a one-worker
  server in a 128 MiB container at a 35 MiB peak. CI runs the attack in a 128 MiB container
  on every change (`slow-uploads` job, `scripts/slow-uploads.sh`). The `max_connections`
  documentation no longer says that the connection limit alone keeps memory bounded;
  `docs/DEPLOYMENT.md` has a section on sizing a container.

### Changed

- **giong-http** (performance): HTTP/2 requests allocate less, and idle HTTPS connections hold
  less memory. An HTTP/2 request made 20 heap allocations in the server besides its
  handler's, now 2 (the request's head and its shared handle). An HTTP/2 connection used to
  give back the memory for its streams (their slots, wakers and queues) each time its last
  stream closed, and allocate it again for the next request. It now keeps it until the
  connection has been idle for a second. The header fields of each request are decoded into
  a buffer the connection reuses, and `content-length` is written without an allocation.
  Measured with `bench/instr` (a giong-http client and server, `GET /json` over HTTP/2):
  66 allocations per request for both ends, now 48, and about 10% fewer instructions. Over
  TLS, the handshake now runs on the heap for its length only, and the connection's task
  no longer holds the TLS stream twice: an idle HTTPS connection holds 10,784 bytes
  instead of 12,400 (rustls, in the process, `idle-https` case). Method, raw rounds and
  where each allocation went: `bench/results/h2-allocs/`.
- **Docs**: a pass over all documentation against the 0.7.0 API. The Rust samples in the
  READMEs, `docs/DEPLOYMENT.md` and `docs/errors/README.md` are now compiled by
  `cargo test -p rest-api --doc`, and run where they need no server; 11 of the 34 did not
  compile before (fragments without their imports or an `async fn`), and three plain-text
  blocks were taken for Rust. Text that still described 0.6: giong-http's README and crate docs called the
  connection cap optional (it is 10,000 per `serve` call), the README's limitations said
  `bind` resolves names with a blocking call (it takes IP addresses only), and
  giong-mysql's `ErrorKind::RowCount`, `Code::MysqlRowCount` and the E3106 page said
  `insert` raises it (it returns `None`). `docs/DEPLOYMENT.md` says how to restart a
  service on the same port now that `bind` refuses a port another process holds.
  `llms.txt` states the address and connection-limit rules. The 0.7.0 migration notes
  below say what implements `net::ToSocketAddrs`, what replaces `version_minor()`, how to
  raise `max_connections`, and that a `match` on giong-redis's `Value` needs a `_` arm.
- **Docs**: a "Choosing crates" section in the README says which crate to reach for: the
  giong crate for the job first, with a table of each one, the Tokio crates it replaces
  and how it measured against them; then crates that need no runtime; then a Tokio crate
  over `giong_compat::Compat` only for a protocol giong has no crate for; and how to spot a
  crate that needs the Tokio runtime. `AGENTS.md`, `llms.txt` and giong-compat's README
  and crate docs say the same, and no longer call the giong crates faster than the same
  protocol through `Compat`, which was never measured.
- **giong-web** (**behaviour change**): `Logger` and `Trace` now record a request that is
  dropped before its response is ready: the client reset its HTTP/2 stream, or a `Timeout`
  layered outside them gave up. Before, such a request left no log line at all and its span
  ended without an event. `Logger` writes a line with `"event":"request cancelled"`,
  `"status":null`, `"bytes":null` and the time until the drop in `duration_us` (nginx logs
  the same case as `499`); `Trace` ends the span with a `request cancelled` event at
  `INFO`. Found by the service built from the documentation alone (F16).
- **giong-mcp**: a worker blocked inside the task giong-http spawns for a connection is now
  reported as a request handler on that connection, with what to look for, instead of only
  the spawn location in giong-http's `src/server.rs`; `profile`'s note and the README say
  that this location holds the time of every request handler. giong cannot yet name the
  handler or route (F2; a request label in `giong::observe` is a 1.x addition).

### Fixed

- **giong-postgres**, **giong-mysql**: conversion errors name Rust types as they are
  written in code (`String`, `Option<String>`), not as `alloc::string::String`, and put
  `an` before a vowel sound (`an integer`, `an i32`; it was `a integer`, `a i32`) (F15).
- **giong-web**: the `cause` of a logged error no longer repeats a source that the error
  above it already shows: a PostgreSQL statement timeout printed the server's message twice
  (F15).
- **giong**: the `Debug` form of an error wrapped from a companion crate, which is what
  `main` prints when it returns one, shows the message once and leaves out an empty `fix`
  (F15).

## [0.7.0] - 2026-11-15

The API freeze for 1.0: the decisions that the 0.6 reviews, and a service an AI agent built
from the 0.6.0 documentation alone, left open, settled. Every item and the reason for each
choice is in [docs/reviews/2026-11-api-freeze.md](docs/reviews/2026-11-api-freeze.md),
with what was kept as it is and what can be added in 1.x without breaking anything. Code
written for 0.7 is meant to build unchanged on 1.0: from here on, `cargo-semver-checks`
checks every change against 0.7.0, and a break before 1.0.0-rc.1 comes only from what
real use in a service turns up.

Changes that break code written for 0.6, by crate; each is marked **Breaking:** below and
says how to migrate:

- **giong**: `bind` refuses an address that another process holds, even one shared with
  `SO_REUSEPORT`; `bind`, `bind_balanced`, `UdpSocket::bind` and `UdpSocket::connect` take IP addresses only
  and refuse a host name with `GIONG-E0100`; `UdpSocket::bind` no longer sets
  `SO_REUSEADDR`; `giong::stats()` is `giong::observe::worker_stats()`.
- **giong-http**: `Request::version_minor` is removed, and `Server::max_connections`
  defaults to 10,000 connections per `serve` call instead of no limit.
- **giong-tls**: `TlsStream::recv` returns `Received`, which derefs to `&[u8]`.
- **giong-postgres**: `Tls` is `#[non_exhaustive]`.
- **giong-mysql**: `insert` returns `Option<u64>`, `None` when no id was generated.
- **giong-redis**: `Tls` and `Value` are `#[non_exhaustive]`, and `mget` takes one type
  parameter.
- **giong-mcp**: `McpServer::url` returns `Option<String>`.

`giong-web` and `giong-compat` have none. Also fixed: giong-mcp's `overview` reported every
worker of a quiet service as fully busy. This release has no benchmark run of its own: the
numbers in the README are those of 0.6.0 and of 0.5.0's bare-metal pass.

### Added

- **giong**: `observe::worker_stats()` and `observe::WorkerStats`, the counters of the
  current worker (live tasks, tracked and in-flight operations, receive buffers, timers),
  documented and `#[non_exhaustive]`. They were `giong::stats()` and `giong::Stats`,
  hidden from the documentation, which the tests of every crate already used to check
  for leaks.
- **giong-http**: `Handler` is implemented for `Rc<H>`, so one handler (a giong-web `App`
  with its pools) can serve a plain port and a TLS port: `let app = Rc::new(app);` and a
  clone for each `serve`. Streaming routes keep streaming, which a closure around the
  handler silently turned off.
- **giong-http**: error code `GIONG-E1119` (`Code::HttpInvalidResponse`) for
  `Response::try_header` and `Response::try_new`, which used `GIONG-E1101`, a code whose
  page is about the client.
- **giong-tls**: `Received`, what `TlsStream::recv` returns (see Changed).

### Changed

- **Breaking:** **giong**: `TcpListener::bind`, `TcpListener::bind_balanced` and
  `UdpSocket::bind` refuse an address that a socket of another process holds, with
  `AddrInUse`, even when that socket shares its port with `SO_REUSEPORT` as giong's own
  do. A service started twice by mistake used to come up twice and split the
  connections (or datagrams) between the two processes. Workers of one process still
  share a port. To share one between processes on purpose, create the sockets yourself
  and pass them to `from_std`. A dropped listener's port can be bound again at once.
- **Breaking:** **giong**: `TcpListener::bind`, `TcpListener::bind_balanced`,
  `UdpSocket::bind` and `UdpSocket::connect` take giong's `net::ToSocketAddrs` with IP
  addresses only, instead of the standard library's trait. A host name
  (`"localhost:8080"`) is refused with `GIONG-E0100`, which says what to write instead:
  the standard library looked it up with a blocking `getaddrinfo` on the worker, outside
  giong's resolver, and `localhost` can mean `127.0.0.1`, `::1` or both. Write the IP
  address, or resolve the name first with `net::lookup_host`. The module no longer has two
  traits named `ToSocketAddrs`. `SocketAddr`, `(IpAddr, u16)`, `&str`, `String`,
  `(&str, u16)` and slices, arrays and `Vec`s of `SocketAddr` implement giong's trait, so
  most calls build unchanged; a function generic over `std::net::ToSocketAddrs` that
  passes its argument on bounds it by `giong::net::ToSocketAddrs` instead.
- **Breaking:** **giong**: `UdpSocket::bind` sets `SO_REUSEPORT` only, not `SO_REUSEADDR`
  as well: with both, a socket of any user that set `SO_REUSEADDR` could bind the port
  and receive its datagrams.
- **Breaking:** **giong**: `giong::stats()` and `giong::Stats` are now
  `giong::observe::worker_stats()` and `giong::observe::WorkerStats` (see Added).
- **giong**: `TcpStream::send_with_control` is hidden from the documentation and outside
  the semver promise: its control bytes are Linux's layout. It stays for `giong-tls`.
- **giong**: error messages link to the error pages of the release that raised them
  (`blob/vX.Y.Z/docs/errors/…`) instead of `main`, which a binary kept for years would
  otherwise point at as the pages change.
- **giong**: the docs of `Sleep` say that it is `Send` on purpose, and stays so.
- **Breaking:** **giong-http**: `Request::version_minor` is removed; `Request::version`
  tells every version apart (it reported `1` for HTTP/2): `req.version_minor() == 0` is
  `req.version() == Version::Http10`.
- **Breaking:** **giong-http**: `Server::max_connections` defaults to 10,000 connections
  per `serve` call (per worker), not unlimited, so that a worker's memory is bounded by
  default. Clients past the limit wait in the listen queue. A worker that holds more
  long-lived connections (event streams, WebSockets) sets
  `Server::new().max_connections(n)`.
- **giong-http**: `Response::header` and `Response::try_header` take the value as
  `impl AsRef<str>`, so a `String` from `format!` needs no `&`.
- **giong-web**: `json(status, value)` with a status outside `200..=599` returns an
  internal error (`500`, with the cause in the log) instead of panicking.
- **Breaking:** **giong-tls**: `TlsStream::recv` returns `Received` instead of
  `Vec<u8>`. It derefs to `&[u8]`, can be sent on as it is (`tls.write_all(data)`), and
  `into_vec()` gives the `Vec`. The type leaves room to hand over kernel TLS's receive
  buffer without a copy later without breaking anyone.
- **Breaking:** **giong-postgres**, **giong-redis**: `Tls` is `#[non_exhaustive]`, as
  giong-mysql's already was, so that ways to check a server can be added; a `match` on it
  needs a `_` arm.
- **giong-postgres**: the `FromSql` docs promise that the list of types read in binary
  does not change within 1.x.
- **Breaking:** **giong-mysql**: `insert` returns `Result<Option<u64>, Error>`: `None`
  when the statement generated no `AUTO_INCREMENT` id, where it used to fail with
  `ErrorKind::RowCount` after the rows were written, so code that retried on errors wrote
  them twice.
- **Breaking:** **giong-redis**: `Value` is `#[non_exhaustive]`, so that a reply type
  added to RESP later is not a breaking change; a `match` on it needs a `_` arm.
- **Breaking:** **giong-redis**: `mget` has one type parameter, the result:
  `mget::<Vec<Option<String>>>(&keys)`, not `mget::<Vec<Option<String>>, _>(&keys)`.
- **Breaking:** **giong-mcp**: `McpServer::url` returns `Option<String>`, `None` on a Unix
  socket, for which it used to return `unix:/path/mcp`, which no MCP client understands.
- **giong-tls**: a certificate refused because it is marked as a certificate authority says
  so, and how to make one that is not. A self-signed certificate from `openssl req -x509`
  is marked that way unless told otherwise; curl accepts it once trusted and rustls does
  not, and the error (`GIONG-E2004`) showed only rustls's debug value,
  `Other(OtherError(CaUsedAsEndEntity))`, with a fix that told the reader to trust the
  certificate they had already trusted. The example API's README, giong-tls's README and
  the E2004 page now give the `openssl` command with `basicConstraints=critical,CA:FALSE`.
- **Docs**: the gaps found by building a service from the published 0.6.0 documentation
  alone. The README has a table for someone coming from Tokio (what takes the place of
  `tokio::spawn`, `join!` and `select!`, `spawn_blocking`, `Arc<Mutex<..>>`, `tokio::sync`,
  `interval` and `ctrl_c`), and `llms.txt` its short form. `examples/rest-api`, the one
  whole service, is listed in `llms.txt`, in `llms-full.txt` and on the site. giong-http's
  README says when a handler's future is dropped (a reset HTTP/2 stream, a timeout, the
  end of a shutdown's grace period) and when it is not (a closed connection), and how to
  serve one handler on two listeners. giong-web's README no longer says that every error
  is problem+json: the `413`, `431` and `400` that giong-http answers before a request
  reaches the app have an empty body and no log line, and a statement timeout is a `500`,
  not a `503`; it also says how to split one prefix over several routers. giong-postgres's
  README no longer says that any column can be read as a `String`, and says that
  `Pool::get` can take every connection and that the pool's check for session changes
  does not see inside functions such as `set_config`.

### Fixed

- **giong**: a dropped UDP socket no longer takes a share of the datagrams meant for a
  socket bound to the same port right after it, during the moment before its close
  reaches the kernel.
- **giong**, **giong-mcp**: a worker with nothing to do no longer reads as fully busy.
  `Pulse::idle` grew only when a sleep ended, so a worker asleep for a minute had no idle
  time to show for it until it woke, and giong-mcp's `overview`, which takes the busy share
  from that figure, reported `busy: 1.0` for every worker of a quiet service (the
  `workers` tool wakes the workers to ask them, and showed 0). The sleep in progress now
  counts: for a sleeping worker `Pulse::idle` grows with the clock, and a later reading is
  never lower than an earlier one. The cost on the worker is unchanged, two clock reads per
  sleep.

## [0.6.0] - 2026-11-15

The first round of reviews before 1.0, fixed. Every crate was reviewed from its code, with
passes of their own over the `unsafe` code and over security, and this release holds the
fixes and adds no feature beyond what they needed. Among them: memory a peer could make a
service hold (HTTP/2 answers and request bodies, Redis replies, giong-mcp batches), session
state that crossed between tasks in the PostgreSQL and MySQL pools, a worker frozen for 18
seconds by one DNS answer, writes lost at the TLS 1.2 record limit, a path traversal
through giong-web's catch-all parameters, descriptors leaked by cancelled opens, and
undefined behaviour in the file and UDP paths that Miri found (nothing miscompiled; Miri
now runs in CI). `giong-mysql` now works with MariaDB 10.6 and later, which it could not
connect to before. The reviews were done by the project's AI agents, not by an outside
auditor, and the findings that need a decision about the API wait for the API freeze
before 1.0.

Three of the bugs fixed here are in 0.5.0 and worth knowing about for anyone who runs it.
There is no 0.5.1; the fixes are in this release only.

- **HTTP/2 downloads to Go clients.** `giong-http`'s server took a client that reads fast
  for a flood: Go's `net/http` was sent GOAWAY (`GIONG-E1117`) after about 500 answers of
  8 KiB on one connection, and again on each new one. curl, h2load, Node.js and
  giong-http's own client were not affected; browsers were not tested. On 0.5.0,
  `Server::http2(false)` avoids it.
- **Kernel TLS on Linux 6.14 and newer.** With kernel TLS turned on (it is off by
  default), a TLS 1.3 connection could stop receiving with `GIONG-E2011` when the peer
  split a handshake message, a session ticket for example, over several records. It
  fails closed: the connection ends with that error.
- **`TcpListener::bind` on Linux 6.0.16 to 6.0.19.** Those four kernels report success
  for a TCP bind to an address in use and leave the socket unbound, so a listener asked
  for a busy `127.0.0.1:8080` came up on another port of every interface (`0.0.0.0`)
  instead of failing. giong now checks the port it got and fails with `AddrInUse`.
  6.0.15 and older and 6.1 and later return the error themselves.

Changes that break code written for 0.5, by crate; each is marked **Breaking:** below and
says how to migrate:

- **giong**: `JoinError` is `#[non_exhaustive]`, and `try_recv` on the `mpsc` receivers
  returns a `Result`.
- **giong-http**: a request whose target is a whole URL must name the host its `Host`
  header names.
- **giong-web**: `Req::param` refuses dot segments, NUL and `/`; `Req::query_param` and
  `Req::query` refuse a repeated key and a bad escape; `Sink` is `#[non_exhaustive]`;
  request ids are 32 hexadecimal digits instead of 20.
- **giong-tls**: `write_all` takes any buffer giong can send, `from_pem` no longer offers
  ALPN `http/1.1`, and `with_roots` with an empty store fails with `NotFound`.
- **giong-postgres**: `Config::statement_timeout` takes an `Option`, the pool refuses
  statements that change the session, and a pooled connection's `CancelToken` stops
  working when the connection goes back.
- **giong-mysql**: `Tls` is `#[non_exhaustive]`, `ssl-mode=VERIFY_CA` needs `ssl-ca` and
  no longer checks the host name, the pool refuses statements that change the session,
  and `Config::session_variable` refuses the variables the crate sets itself.
- **giong-redis**: `ToArg::write_arg` replaces `ToArg::with_bytes`.
- **giong-mcp**: a TCP server needs a token, and `start_from_env` returns `FromEnv`.

`giong-compat` has none. Some defaults changed without breaking a build: the number of
workers follows a container's CPU quota, `giong-mysql` caches 64 statements per connection
instead of 256 and refuses servers older than MySQL 8.0 and MariaDB 10.6, `giong-redis`
counts a reply's elements against `max_reply_size`, and an HTTP/2 request body larger than
about 5 KiB waits one round trip for more window.

The two small losses that 0.5.0's bare-metal pass found came from zero-copy send, which
had made every TCP write future larger, and most of both is taken back: an idle HTTP/2
connection costs 4.33 KiB (4.55 in 0.5.0, 4.30 before zero-copy send), and a small request
over HTTPS runs 15,618 instructions in the server (15,715 in 0.5.0, 15,571 before). These
are measured on a 4-core virtual machine (`bench/results/instructions/`). This release
has no bare-metal run of its own: the two machines of the 0.5.0 pass are no longer
rented, so the bare-metal numbers in the README are those of 0.5.0.

### Added

- **giong-http**: `Server::http2_prior_knowledge(false)` keeps HTTP/2 to TLS connections
  that choose it through ALPN, so that a plain port behind an HTTP/1.1 proxy speaks only
  HTTP/1.1. `Server::http2(true)` used to turn on both, and still does by default.
- **giong-http**: `Response::try_header` and `Response::try_new`, the forms of
  `Response::header` and `Response::new` that return an error instead of panicking on a
  header name or value that is not valid (a line break that would split the response)
  or a status outside 200 to 599, for values that come from the request: a redirect
  target from a decoded query parameter, a file name in `Content-Disposition`. The error
  is a `giong::error::Error` with the code `GIONG-E1101` (`Code::HttpClientRequest`, the
  code giong-http already gives a message that cannot be sent as written). `header` and
  `new` are unchanged and their documentation points to the new forms.
- **giong-web**: `Error::from_status(status, code, detail)` and `Error::try_with_header`,
  forms of `Error::new` and `with_header` that never panic, for a status or a header
  value that comes from an upstream service or the request: a status outside
  `400..=599` becomes `502` with the one given in the logged cause, and an invalid header
  gives the error back without it. The docs say which form takes which values, and
  explain `giong_web::json` (serializes a value) against giong-http's `Response::json`
  (takes JSON already written).
- **giong-tls**: a fuzz target, `ktls_records`, for what kernel TLS reads of the peer's
  records outside the kernel (the record framing during the handshake, and handshake
  messages after it), in the nightly fuzz workflow; a unit test runs the same checks on
  3,000 random streams. 1.65 million inputs ran without a failure when it was added.
- **giong-postgres**: `Pool::idle_timeout` (10 minutes by default) closes connections
  left unused, and `Pool::max_lifetime` (30 minutes, less up to 10% so that connections
  opened together are not replaced together) replaces old ones once no statement uses
  them. A connection whose server refused a write because it is a standby (`25006` while
  `in_hot_standby` is on, as after a failover that kept the old primary up) is replaced
  too. `PoolStats::recycled` counts them. Connections used to live until the server or
  the network closed them.
- **giong-postgres**: `Config::require_auth(&[AuthMethod])`, and `require_auth` in a URL
  with libpq's method names, accept only the authentication methods listed, so that
  without TLS an impostor cannot ask for MD5 instead of SCRAM or let the client in without
  asking. `DbError::routine` gives the server's routine field.
- **giong-mysql**: MariaDB 10.6 and later. Every connection to MariaDB used to fail with
  `GIONG-E3101`, on the session's `max_execution_time`, which MariaDB does not have.
  MariaDB is told apart by its version string (`5.5.5-10.11.14-MariaDB`, `11.4.4-MariaDB`);
  the handshake asks for `utf8mb4_general_ci`, since MariaDB has no `utf8mb4_0900_ai_ci`,
  and the statement timeout is set as `max_statement_time`, which MariaDB applies to every
  statement, writes and DDL included. MySQL before 8.0 and MariaDB before 10.6 are
  refused before logging in, naming the versions supported (`ErrorKind::Connect`). The
  integration tests run on MySQL 8.0.46 and MariaDB 10.11 locally, and CI adds MariaDB
  10.6, 10.11, 11.4 and 11.8 to MySQL 8.0, 8.4 and 9.1. MariaDB's `client_ed25519` login
  is not supported.
- **giong-mysql**: `Tls::verify_ca(pem)` checks the server's certificate against the given
  authorities but not against the host name, for the certificates MySQL generates when
  it is installed, which name no host
  (`MySQL_Server_<version>_Auto_Generated_Server_Certificate`) and so could not be used
  with any mode before. The chain, the validity period and the key usage are still
  checked: a certificate of another authority, an expired one, or the authority's own
  certificate offered as the server's are refused.
- **giong-mysql**: `Client::reset_session`, which resets the session the way the pool does
  when a connection comes back, and three `PoolStats` fields: `draining` (connections set
  aside for tasks waiting in `get`), `resets` and `statement_limit_errors` (prepares the
  server refused for `max_prepared_stmt_count`).
- **giong-mysql**: a fuzz target, `session`, that runs a client with queued statements,
  batches, transactions, dropped futures and a pool's sharing rules against a server
  made of the fuzzer's bytes, in memory, and checks that every request ends, that nothing
  is left in flight, and that what the client wrote is whole packets of known commands.
  590,000 runs in 5 minutes found no crash. CI fuzzes it with the other targets.
- **giong-redis**: a fuzz target for the connection's reply matching (`connection`, in
  `giong-redis/fuzz/`, and a randomised test): commands of every shape, dropped waiters
  and passed deadlines, with push messages, attributes and replies over the limit among
  the replies, which arrive cut anywhere; every command must get its own replies. The
  reply reader's fuzz inputs now carry attributes, and its results are checked against a
  reading with no limit.
- **giong-mcp**: `Config::allow_unauthenticated()`, the explicit way to serve TCP without a
  token; `McpServer::unix_path()`; the `FromEnv` that `start_from_env` returns; error codes
  `GIONG-E1303` (`McpBind`: the address cannot be listened on, with the address and
  `GIONG_MCP` in the message; the kernel's error kind is kept), `GIONG-E1304`
  (`McpNoToken`) and `GIONG-E1305` (`McpBadToken`). Each tool declares the shape of its
  answer as an `outputSchema`, and the README states that this JSON follows the crate's
  semver from 1.0. A cargo-fuzz target, `request`, sends any bytes through the server's
  checks and JSON-RPC dispatch, in CI with the other targets.
- **Verification**: fuzz targets for the core crate (`fuzz/`): `dns_response` (DNS answers,
  up to 64 KiB, with a 2-second limit per input so that a slow answer counts as a failure)
  and `resolv_conf` (`/etc/resolv.conf` and `/etc/hosts`), in the nightly fuzz workflow.
- **Verification**: a fuzz target for the HTTP/1.1 client reading a response on a
  connection, `response_body` (in the nightly fuzz workflow): whatever a server sends, in
  whatever pieces, the head and body must be what the bytes hold, the body ending exactly
  where its length, its chunked framing or the close says (or failing with the error
  that framing calls for), and the connection may be reused only when the bytes received
  end exactly at the end of the body. The reading code now takes its bytes through a
  small trait, so the target drives the code the client runs. 100,000 responses run in
  `cargo test`.

### Changed

- **Breaking:** **giong**: `JoinError` is `#[non_exhaustive]`, so that a reason can be
  added without breaking code; a `match` on it needs a `_` arm.
- **Breaking:** **giong**: `mpsc::Receiver::try_recv` and
  `mpsc::BoundedReceiver::try_recv` return `Result<T, mpsc::TryRecvError>` instead of
  `Option<T>`, like `oneshot::Receiver::try_recv`: `Empty` while a sender is alive,
  `Closed` once every sender is gone and the channel is empty. The `Option` could not tell
  the two apart.
- **giong**: the default number of workers follows the CPU quota. It was the number of CPUs
  in the process's affinity mask, which in a container with a CPU limit is every CPU of the
  machine: a Kubernetes pod with `limits.cpu: 2` on a 64-core node ran 64 pinned workers
  that the quota throttled. It is now the smaller of the affinity mask and the quota
  (`std::thread::available_parallelism`), so 2 there. Under such a quota workers are no
  longer pinned by default, since the CPUs are shared with other processes and pinning
  stacked the workers on CPUs 0 and 1; `Builder::pin_cores(true)` pins them anyway.
- **giong**: the panics an agent or a person meets first now carry a code, the cause and
  the fix, like giong's errors: `GIONG-E0011` (a giong function called outside a giong
  runtime, for example on Tokio's executor), `GIONG-E0012` (`block_on` inside a runtime)
  and `GIONG-E0013` (a giong future polled on another worker).
- **giong**: `OpenOptions::custom_flags` ignores the access-mode bits (`O_RDONLY`,
  `O_WRONLY`, `O_RDWR`), as the standard library does: `custom_flags(libc::O_RDWR)` no
  longer turns a read-only open into a writable one. Combinations `open(2)` cannot do (no
  access mode, `truncate` or `create` without `write`, `append` with `truncate`) fail with
  `GIONG-E0203`, which names the option and the fix, instead of a bare `EINVAL`.
- **giong**: `File::allocate` with a length of 0 does nothing and returns `Ok` (it was the
  kernel's `EINVAL`), and a range that ends above `i64::MAX` is refused with `GIONG-E0202`.
- **giong**: the docs of `TcpStream` and `UnixStream` `read`, `write` and `write_all` say
  that they are not cancel safe, and `read` that a buffer with no capacity reads `Ok(0)`.
- **Breaking:** **giong-web**: `Req::query_param` and `Req::query` follow one strict rule.
  `query_param` used to take the first of a repeated key and skip a key that did not
  decode; it now answers `400` (`GIONG-E1205`) for a repeated key it is asked for and for
  a bad escape anywhere in the query string, as `query` did. `query` into a map
  (`HashMap`, `serde_json::Value`) refuses a repeated key instead of keeping the last
  value; a struct still ignores the keys it does not know.
- **Breaking:** **giong-web**: `Sink` is `#[non_exhaustive]`, so that sinks can be added
  without breaking a `match`.
- **Breaking:** **giong-web**: request ids made by `RequestId` are 32 hexadecimal digits
  instead of 20: 64 random bits per process instead of 32, 16 bits for the worker instead
  of 8, then a 48-bit counter. With 32 random bits, two processes of a fleet shared ids
  with even odds after about 77,000 starts, and workers above 256 shared them within a
  process. Code that checks an id's length, or a column sized for it, needs the new one.
- **giong-web**: `Logger` cuts a path or query string longer than 4 KiB in its lines,
  ending `...(+N bytes)`. Sixteen requests with a 64 KiB path filled a worker's 1 MiB
  log buffer, and the lines after them were dropped.
- **giong-web**: the log writer shrinks a worker's buffer back to 64 KiB once written,
  instead of keeping the memory of a burst (up to 1 MiB per worker and stream), and
  removes the buffer of a thread that ended, after writing its last lines, instead of
  keeping one per thread that ever logged.
- **giong-web**: the `Timeout` docs no longer say that every giong operation is safe to
  cancel: dropping is memory-safe, the database and HTTP clients close a connection whose
  request was abandoned, and some I/O (a vectored write, `send_file`) may be cut halfway.
  They show how to limit the work a streaming route does once its body is in, which
  `Timeout` does not cover.
- **Breaking:** **giong-tls**: `TlsStream::write_all` takes any buffer giong can send (`B:
  giong::IoBuf + AsRef<[u8]>`: `Vec<u8>`, `&'static [u8]`, `String`, `Box<[u8]>`,
  `Rc<[u8]>`, `Arc<[u8]>`, ...) and hands it back, as `write_all_vectored` already did.
  Callers that pass a `Vec<u8>` are unchanged.
- **Breaking:** **giong-tls**: `TlsAcceptor::from_pem` no longer offers ALPN `http/1.1`. A
  server that offered it refused, with `no_application_protocol`, every client that
  offered ALPN without `http/1.1` (an h2-only client, any other protocol). Add it with
  `.alpn(&[b"http/1.1"])` where it is wanted; giong-http's `serve_tls` sets its own.
- **Breaking:** **giong-tls**: `TlsConnector::with_roots` with an empty store fails with
  `ErrorKind::NotFound`, as `GIONG-E2001` does everywhere else, instead of `InvalidData`.
- **giong-tls**: `shutdown()` sends the FIN on every path: in the process after a write
  that failed it now closes the TCP side and returns `Ok`, as kernel TLS already did,
  instead of failing before the FIN. After `shutdown()`, a write in the process fails
  with `GIONG-E2008` (`TlsBroken`) instead of reaching rustls.
- **giong-tls**: connections keep their traffic secrets extractable only when they may
  be handed to the kernel (`Ktls::Auto` or `Required` on a kernel with the `tls`
  module), not on every connection of a `Ktls::Auto` configuration.
- **giong-tls**: needs rustls 0.23.33 or newer (was 0.23.27) and libc 0.2.172 (was any
  0.2). Before 0.23.33, rustls allowed a peer 32 `KeyUpdate` messages for the whole life
  of a connection, so a long-lived TLS 1.3 connection whose peer changes keys regularly
  failed with `TooManyKeyUpdateRequests`; from 0.23.33 they are counted only between
  application data. Checked by building and testing with the lowest versions allowed
  (`cargo update -Z direct-minimal-versions`).
- **giong-compat**: the lowest dependency versions it allows are ones it builds and passes
  its tests with (`cargo update -Z direct-minimal-versions`): tokio 1.13 (was any 1.x),
  futures-io 0.3.31 (was any 0.3), and for the tests rustls 0.23.33, the same as
  giong-tls, and hyper 1.6, the first whose header-read timeout reports `is_timeout()`.
- **Breaking:** **giong-postgres**: `Config::statement_timeout` takes `impl
  Into<Option<Duration>>`. `None` sends nothing, leaving the limit to the server or the
  role, which is what PgBouncer needs. `Duration::ZERO` and values above `i32::MAX`
  milliseconds are a `Config` error when connecting; zero used to become 1 ms, and the
  server refused the larger ones.
- **Breaking:** **giong-postgres**: the pool's statement methods refuse statements that
  change the session (`SET` without `LOCAL`, `RESET`, `DISCARD`, `LISTEN`, `UNLISTEN`,
  `PREPARE`, `DEALLOCATE`, `DECLARE`, `LOAD`, `CREATE TEMP ...`) and `COPY ... FROM STDIN`
  before sending them, with `ErrorKind::Statement` (`GIONG-E3011`); `batch_execute` too,
  which still accepts transactions. They belong on a connection from `Pool::get`, or in
  `Config::param`.
- **Breaking:** **giong-postgres**: a `CancelToken` taken from a `PooledClient` stops
  working when the connection goes back to the pool: `cancel()` sends nothing and fails
  with `ErrorKind::Closed`. It used to cancel whatever another task ran on the connection
  by then. Tokens no longer carry the password or the SCRAM keys.
- **giong-postgres**: a `COMMIT` that the server turned into a rollback is still
  `ErrorKind::Db`, now with `code()` `25P02`; it had no code.
- **giong-postgres**: prepared statements are named `__giong_1`, `__giong_2`... instead of
  `g1`, `g2`..., which an application's own `PREPARE` could take; on a name taken all the
  same (`42P05`), the next name is used.
- **giong-postgres**: SCRAM-SHA-256's key derivation (4,096 rounds of HMAC-SHA-256 by
  default, more if the server asks) runs on a thread of its own instead of the worker,
  whose other tasks it stalled, and its result is kept for the next connection with the
  same password, salt and round count.
- **Breaking:** **giong-mysql**: `Tls` is `#[non_exhaustive]`, so that ways to check a
  server (pinning, a client certificate) can be added later; a `match` on it needs a `_`
  arm.
- **Breaking:** **giong-mysql**: `ssl-mode=VERIFY_CA` in a URL now checks the certificate
  against `ssl-ca` and not against the host name, as in MySQL's own client, and needs
  `ssl-ca`; without it, it is an error. `ssl-mode=REQUIRED` with `ssl-ca` does the same.
  `REQUIRED` without `ssl-ca` still checks the certificate and the host name against the
  machine's authorities, which is stricter than MySQL's client (it checks nothing then).
  Both modes used to check the host name, so they could not be used with MySQL's own
  certificates.
- **Breaking:** **giong-mysql**: the pool's shared methods (`Pool::query`, `execute`,
  `insert`...) now also refuse statements that change the session before sending them:
  `SET` (but `SET GLOBAL`), `USE`, temporary tables, `GET_LOCK` and assignments to user
  variables (`ErrorKind::Statement`, `GIONG-E3111`). Other tasks' statements would have
  run in the changed session. A `Pool::batch_execute` that may change the session, and any
  shared statement the server reports changed it (through a trigger or a stored
  procedure), are followed by a reset at once.
- **Breaking:** **giong-mysql**: `Config::session_variable` refuses the variables
  giong-mysql now sets itself: `character_set_*`, `collation_connection`, `names`,
  `autocommit`, `max_statement_time` and `session_track_*`, besides `time_zone` and
  `max_execution_time` as before. `lock_wait_timeout` may be set, and replaces the one
  taken from the statement timeout.
- **giong-mysql**: the statement cache keeps 64 statements per connection by default,
  down from 256. The server's `max_prepared_stmt_count` (16,382 by default) bounds the
  statements of all its clients together: at 256, 64 connections (2 processes × 8
  workers × a pool of 4) filled it, and from then on every prepare on the server failed
  with error 1461, for every application using it. 64 allows 255 connections. When the
  limit is reached anyway, the connection closes its statements that are not in use and
  prepares again, once, and `PoolStats::statement_limit_errors` counts it. Error
  1615 (`NEED_REPREPARE`) now closes the stale statement on the server too, instead of
  only forgetting it.
- **giong-mysql**: a connection taken with `Pool::get()` and given back with `autocommit`
  off is reset, like any other whose session changed, instead of being closed.
- **giong-mysql**: `Pool::get()` sets a connection aside for each waiting task (no new
  shared statement goes on it) and hands it over as soon as the statements already on it
  are answered; tasks are served in order, and when the pool may open more than one
  connection, one is always left to the shared statements. A task in `get` used to wait
  for a connection with nothing in flight, which shared statements could keep from ever
  happening, until the wait timeout.
- **giong-mysql**: the statement timeout also bounds waiting for a table or metadata lock
  (`LOCK TABLES` elsewhere, a running `ALTER TABLE`): it is set as the session's
  `lock_wait_timeout`, rounded up to whole seconds, on MySQL and MariaDB. Such a write
  used to wait as long as the server's default allows: a year on MySQL. On MySQL a
  write that holds its locks and runs long is still not stopped; on MariaDB,
  `max_statement_time` stops it.
- **Breaking:** **giong-redis**: `ToArg` writes through an `ArgWriter`: `fn
  write_arg(&self, out: &mut ArgWriter<'_>)` replaces `fn with_bytes(&self, f: &mut dyn
  FnMut(&[u8]))`. Everything an implementation writes, in any number of pieces, is one
  argument (`out.write(b"session:")` then `self.0.write_arg(out)` sends `session:42`), so
  an implementation can no longer break the framing of a shared connection (see Fixed).
  `ArgWriter` implements `std::fmt::Write` too. To migrate, call `out.write(bytes)` where
  the old implementation called `f(bytes)`.
- **giong-redis**: `Config::max_reply_size` now bounds the memory a reply takes: each
  element counts 48 bytes (the `Value` it becomes) besides its bytes. Counting bytes only,
  an array of 20 million nulls, 57 MiB as sent and so under the 64 MiB default, took
  960 MB once read. A reply of millions of small elements that fit before may now fail
  with `TooLarge` (a million 10-byte members, 17 MB as sent, count for 65 MB): raise the
  limit for such replies.
- **giong-redis**: when 16 MiB of commands are waiting to be written to a connection (the
  server stopped reading, as during a long script), a new command waits for room, and
  fails with `Timeout`, unsent, if none comes within the response timeout. Before, the
  buffer grew without bound, and commands whose callers had given up were sent, and run,
  whenever the server caught up.
- **Breaking:** **giong-mcp**: a TCP server needs a token. Any local process can connect
  to a loopback port, whichever user runs it, so `Config::tcp` without `token` fails to
  start with `GIONG-E1304` unless the code calls `allow_unauthenticated()`; a Unix socket
  needs none. A token must be at least 16 characters a bearer token can carry:
  `token("")`, or `GIONG_MCP_TOKEN` set but empty, is refused with `GIONG-E1305` instead
  of quietly turning authentication off. The `Bearer` scheme name is matched in any case
  (RFC 9110).
- **Breaking:** **giong-mcp**: `start_from_env` returns `FromEnv` instead of
  `io::Result<Option<McpServer>>`, and never stops the service: a busy port or a bad
  `GIONG_MCP` is written to stderr and kept in the value, and the service runs without the
  endpoint. `FromEnv` and `McpServer` are `#[must_use]`, so `start_from_env(&h);`, which
  used to print "listening" and stop the server at once, is a warning, and
  `start_from_env(&h)?;` no longer compiles. Remove the `?`.
- **giong-mcp**: a tool's text copy is compact JSON instead of pretty-printed (34% fewer
  bytes for `overview`, 42% for `workers` at four workers), and `profile` is no longer
  annotated `readOnlyHint`, since it makes every worker time its polls while it runs.
  `Config::snapshot_timeout` is at most 10 s.
- **Docs**: `llms.txt` on giong.run links the documentation of the release it names
  (`blob/vX.Y.Z/`, and docs.rs at that version) instead of `main`, which already described
  features no published crate had, so an agent following it wrote code that did not
  compile against what `cargo add` installs. It lists every released crate's README and
  says how to turn giong-mcp on. `llms-full.txt` is new: every crate's README, the
  deployment notes and every error page of the release in one file, read from the tag.
  The commit scopes in AGENTS.md and CONTRIBUTING.md cover every crate.

### Fixed

- **giong**: `TcpListener::bind` to an address in use fails with `AddrInUse` on Linux 6.0.16
  to 6.0.19 too. Those kernels return 1 instead of -1 from bind(2) on a TCP socket whose
  address is taken and leave the socket unbound, which read as success: the listener then
  came up on a free port of every interface (`0.0.0.0:43289` when asked for a busy
  `127.0.0.1:40079`). giong now checks the port the socket ended up on. Other kernels, UDP
  and Unix sockets never had this.
- **giong**, epoll driver: file operations no longer queue behind one that blocks. A burst
  of file operations started while one blocking thread was idle all went to that thread, so
  one open of a FIFO without a writer (or a read from a hung network filesystem) stalled
  every file operation of the process. A thread now starts whenever more jobs wait than idle
  threads can take.
- **giong**: work sent through a `Handle` to a runtime that has stopped, or was dropped
  without running, is cancelled instead of waiting forever: `spawn_balanced` after `run`
  returned, jobs still queued when the last worker stopped, and `spawn_on` to a runtime that
  never ran now resolve to `JoinError::Cancelled`, as documented.
- **giong**: with `shutdown_on_signals`, SIGINT and SIGTERM are seen as long as any worker
  runs. Only worker 0 watched for them, so once its main future had returned the signals
  did nothing and the process had to be killed.
- **giong**: `Cargo.toml` asked for `libc` 0.2, but giong needs 0.2.172 (what socket2 0.6
  needs), so a lockfile holding an older `libc` failed to build. CI now builds and tests the
  core crate with every direct dependency at the lowest version its `Cargo.toml` allows
  (`minimal-versions` job), and a test checks that I/O completes while another task on the
  worker keeps yielding, which io-uring 0.7.0 to 0.7.14 broke before 0.5.0.
- **giong**: a balanced listener could leave a connection in the mailbox of a worker whose
  last balanced listener closed at the same moment, open and unanswered. The hand-over now
  checks under the mailbox's lock and keeps the connection, or gives it to another member.
- **giong**: a DNS answer with a long chain of CNAME records froze the worker. The chain was
  followed by rescanning every record for every name found, in cubic time: one 64 KiB TCP
  answer of 2,300 records held the worker for 18 seconds, with no timer or other task
  running, and whoever controls DNS answers (a resolver, or anyone on the path to it) could
  send one. The chain is now followed from the query name in linear time, and more than 16
  CNAME links is a malformed answer (`GIONG-E0107`), as in other resolvers.
- **giong**: `File::read_at`, `write_at` and `write_all_at` refuse an offset above
  `i64::MAX` with `GIONG-E0202`. io_uring reads `u64::MAX` as "the current file position",
  so such an offset read and wrote wherever that was, on io_uring only (epoll failed with
  `EINVAL`).
- **giong**: a DNS server address where nothing listens gave the kernel's bare "connection
  refused", so a failed `connect("db.internal:5432")` read as if the database refused. It
  is now `GIONG-E0108`, naming the server and the name, with the kernel's error as its
  source. `GIONG-E0104` says when it really applies (an empty `ResolverConfig::nameservers`;
  a `resolv.conf` without servers means `127.0.0.1:53`).
- **giong**: `UdpSocket::read_from` refuses a buffer with no capacity (`Vec::new()`) with
  `GIONG-E0109`, instead of receiving the datagram into it and losing it.
- **giong**: an IP address without a port (`"10.0.0.5"`, `"::1"`) or an IPv6 address
  without brackets (`"2001:db8::1:8080"`) passed as `"host:port"` now gets an error that
  says so and shows the right form, instead of "not a valid host name".
- **giong**: a `File::open`, `File::create` or `OpenOptions::open` whose future was dropped
  after the open started (a timeout, a cancelled request) leaked the descriptor the open
  produced: on io_uring every one (200 of 200 in the new test), on epoll those already
  running. The driver now closes the result of an open nobody collects, on both drivers.
- **giong**: memory the kernel writes into during an operation (the `statx` result, the
  `msghdr` of `UdpSocket::read_from` and `send_to`, the headers of vectored and control
  sends, the address of `connect`, and paths) was reached through a pointer taken before its
  `Box` moved, which Rust's aliasing rules make undefined behaviour (nothing miscompiled,
  but Miri failed on every file test). It now lives in a small owner reached only through a
  raw pointer, and `IoBufMut for Vec<u8>` hands the kernel a pointer valid for the whole
  capacity. So that this stays fixed, CI runs the unit tests and the tests Miri can run
  (files, tasks, channels, timers, `observe`, several workers) under Miri on the epoll
  driver (`scripts/check.sh miri`), and clippy checks that every `unsafe` block and impl
  in giong says why it is sound.
- **giong**: the buffer ring's entries are written field by field through raw pointers.
  The setters of `io_uring`'s `BufRingEntry` take a reference to the whole entry, whose
  last field in entry 0 is the ring tail the kernel reads at any time (nothing
  miscompiled). The `# Safety` sections of `IoBuf` and `IoBufMut` now also say that
  nothing else may write the bytes (for `IoBufMut`, read or write them) while giong holds
  the buffer, which the implementations giong provides already guaranteed.
- **giong**: `observe` grouped tasks by the address of their spawn `Location`, so one call
  site could show up as several entries; it groups by file, line and column.
- **giong**: a stream or listener dropped by code the driver runs (a waker's `wake` or
  `drop`) kept its multishot receive or accept: the kernel kept the socket open, so the
  peer never saw it close, and the driver kept the entry. An operation's future dropped
  there leaked its buffer and its entry. Both are now finished at the next turn of the
  event loop.
- **giong**: a `TcpListener` or `UnixListener` that outlived its runtime (returned from one
  `block_on` and used in the next) leaked the connections io_uring had accepted for it but
  nobody had taken: the descriptors stayed open and the clients waited. The runtime closes
  them, and gives back the receive buffers of every live multishot queue, when it stops.
- **giong**: a failure of `io_uring_enter`, of submitting to the ring or of `epoll_wait`
  that the event loop cannot recover from still stops the runtime, but the panic is now
  coded `GIONG-E0005` and says what the error means: `EBADR` and `ENOMEM` that the kernel
  ran out of memory for completions, anything else that it is worth a report.
- **giong**: on the epoll driver, reading or writing a pipe on standard input, output or
  error through `fs::File` switched the pipe to non-blocking mode, which belongs to the
  pipe and so changed it for the rest of the shell pipeline too (`cat` on the same pipe
  failed with `EAGAIN`). Those pipes are now read and written on the blocking threads, as
  terminals already were; pipes on other descriptors still go through epoll.
- **giong**: an idle HTTP/2 connection costs what it did before 0.5.0's zero-copy send. That
  change made every TCP write future 48 to 88 bytes larger (a wide pointer for the hold,
  and the retry for sockets that refuse zero-copy kept alive across the send), and
  `giong-http` keeps several in each connection, so an idle HTTP/2 connection went from
  4.30 to 4.55 KiB. The hold is now a thin pointer, an operation's driver key is 32 bits,
  and an operation sends the same bytes again with a copy itself when the socket refuses
  zero-copy, so a write of one buffer is 8 bytes larger than before zero-copy send and an idle
  HTTP/2 connection costs 4.33 KiB (5,000 idle connections, 1 worker, RSS, `bench/giong-http`
  with `bench/hold h2-idle`, on a 4-core VM; the bare-metal pass measured 3.99 and 4.21
  KiB before and after). The same change takes back most of the instructions zero-copy
  send had added to each small request: over rustls 15,571 before it, 15,715 with it,
  15,618 now; over plain HTTP/1.1 9,109, 9,242, 9,130 (`/json`, counted by the new
  `bench/instructions.sh`, results in `bench/results/instructions/`). That was the
  largest part of the 2% fewer small HTTPS requests the bare-metal pass measured.
- **Breaking:** **giong-http**: an HTTP/1.1 request whose target is a whole URL (absolute
  form, `GET http://host/path`, which servers must accept) was served whatever its scheme
  (`foo://x/y` too), and its host was ignored: the handler saw `path() == "/path"` and the
  host in `Host`, which could name another one than the URL a proxy in front routed or
  authorised on. Such a target must now be `http` or `https` (in any case), carry no user
  name or password, and name the host the `Host` header names (compared without regard to
  case; a port on one side only is a difference), or the request is answered with `400`,
  as HTTP/2 already did for a `host` that differs from `:authority`. An HTTP/1.0 request
  in absolute form without `Host` is refused too, `*` is accepted only with `OPTIONS`, and
  a target that is none of path, URL, `*` or (with `CONNECT`) `host:port` is refused. The
  path of such a URL now ends where its query starts (`http://h?to=/next` had the path
  `/next`).
- **giong-http**: an HTTP/2 client could make the server hold a whole answer for each of
  its streams. Handlers ran as soon as their requests arrived, and their response bodies
  waited in memory for the client's flow control, with no limit: a client that set its
  window to 0 and opened 100 streams to an endpoint answering 1 MiB made the server hold
  100 MiB per connection for `write_timeout` (5 such connections: 2.8 to 505 MiB of
  resident memory). Handlers now run only while the response bytes a connection holds,
  answered and not sent yet, are within a budget (the larger of `max_body_size` and
  1 MiB); past it the connection's other handlers wait, those already running included,
  until the client takes enough or the stream it does not take is reset. The same test
  now holds 1.1 MiB per connection. A `&'static` body does not count, and a file only for
  the 64 KiB piece of it being sent.
- **giong-http**: pending HTTP/2 request bodies were not bounded by the body budget the
  documentation promised (the larger of `max_body_size` and 1 MiB): every stream could
  send the protocol's 64 KiB whatever the budget, so 100 streams held 6.4 MiB per
  connection (measured: 7.2 MiB, against 1.2 MiB for HTTP/1.1). The server now announces
  a smaller first window (`SETTINGS_INITIAL_WINDOW_SIZE`: half the budget divided by
  `max_concurrent_streams`, 5,242 bytes by default, within 4 and 64 KiB), sets aside the
  first windows of streams that may still open before it grants more, and, until the
  client has acknowledged its settings, refuses (`REFUSED_STREAM`) streams with a body
  beyond what the budget holds at 64 KiB each. A connection now holds at most the budget
  plus 256 KiB (measured: 1.65 MiB). A request body larger than the first window waits
  one round trip for more, as one larger than 64 KiB did before.
- **giong-http**: a handler that panicked took its whole connection down, and over
  HTTP/2 that meant every other request on it, without a GOAWAY. Its request is now
  answered with `500 Internal Server Error`: over HTTP/2 only that stream ends and the
  connection goes on; over HTTP/1.1 the connection closes after the `500` instead of
  being cut without an answer. The same holds for a panic in `Handler::stream_body`.
- **giong-http**: 0.5.0 closed HTTP/2 connections of clients that only read their answers
  fast, with GOAWAY `ENHANCE_YOUR_CALM` (`GIONG-E1117`). A WINDOW_UPDATE that arrived when
  the rest of the answer was already sent, or its stream closed, counted as a frame that
  does no work, of which 1,000 and then 500 a second are allowed. Go's `net/http` client
  gives window back for every 4 KiB it reads, on the stream and on the connection: with
  32 requests at a time on one connection it was cut off after about 2,000 answers of
  2 KiB, 500 of 8 KiB or 30 of 1 MiB, and again on each new connection (it sends the
  requests again, so few failed, but every one of them is a connection lost). Ten such
  WINDOW_UPDATE frames for each DATA frame sent are no longer counted; more than that
  still is a flood. Clients that give window back in larger steps were not affected:
  curl, h2load and Node.js (nghttp2), and giong-http's own client.
- **giong-http**: `Handler::stream_body` said that answering early closes the connection;
  over HTTP/2 only the stream ends.
- **giong-http**: over HTTP/2, a `host` field that names the `:authority` host in another
  case is no longer refused (host names are case-insensitive, and HTTP/1.1 now compares
  the same way), and an `:authority` with a user name or password is (RFC 9113, section
  8.3.1).
- **giong-http**: trailer lines after a chunked body were dropped without being looked at,
  so a line without a colon, with a bare CR in it, or with a name that is not a token
  was accepted, which a proxy in front that ends lines at a bare CR would read
  differently. They are now held to the rules of a header line: the server answers
  `400` (`GIONG-E1113` for a body read as a stream), and the client, which shares the
  decoder, fails with `GIONG-E1107` (`ErrorKind::Protocol`).
- **giong-http**: the errors of a request body read as a stream, and of a response body
  that can no longer be sent, ran their fix into the link after it ("... the limit
  `Handler::stream_body` returns See https://..."): their fixes now end with a full stop.
- **Breaking:** **giong-web**: `Req::param` refuses with `400` (`GIONG-E1205`) a value
  that once percent-decoded has a `.` or `..` segment (sent as is or as `%2E%2E`/`..%2F`;
  giong-http does not remove dot segments), a NUL, a `/` in a one-segment parameter, or,
  for a catch-all, a leading `/`. `/files/{*path}` joined to a directory was a path
  traversal. The new `Req::param_raw` gives the value as sent, unchecked, for the route
  that wants it.
- **giong-web**: a handler or middleware that panicked took its connection down with no
  response, no log line and no request id, and over HTTP/2 every other request on that
  connection with it. A panic, in the handler, in a closure before it returns its future
  or in a middleware, is now caught where it happens and answered like any other failure:
  `500` problem+json with `GIONG-E1210`, whose logged cause is "the handler panicked:
  ..." or "a middleware panicked: ...", so `Logger`, `Trace` and `RequestId` see an
  ordinary error response. A header value from the request that makes
  `Response::header` panic (`%0A` in a query parameter) is one such case.
- **giong-web**: middleware layered on a nested router never saw the requests giong-web
  answers by itself, so `Cors` on the router nested at `/api` never answered a preflight
  (browsers then blocked every JSON or authenticated cross-origin call to those routes),
  and errors under the prefix had no `Access-Control-Allow-Origin`. A CORS preflight now
  runs through the middleware of the route its `Access-Control-Request-Method` would go
  to; another automatic `OPTIONS` or a `405`, through those of the first route at the most
  specific path that matches; a `404`, through those of the router nested at the most
  specific prefix of the path (the outermost router's elsewhere, as before).
- **giong-web**: `Allow`, in a `405` and in the automatic answer to `OPTIONS`, named the
  methods of the most specific matching path only, though the method picks among every
  path that matches (`DELETE /users/me` goes to `DELETE /users/{id}`). It now lists every
  method that would route to the URL. The rule is now in the README.
- **giong-web**: the docs of `Req::json` showed a `&str` field, which serde can only lend
  when the string needs no unescaping, so valid JSON with `\"` or `\u00e9` (and a query
  value with `+` or `%XX`) was answered `422`/`400`, blaming the client. The examples now
  use `String` or `#[serde(borrow)] Cow<'a, str>`, and such a failure is a `500` whose
  logged cause (`GIONG-E1213`) says to use one of them.
- **giong-web**: `?` looked for a giong code in the top error only, and read it from the
  message of any error, `io::Error` included. A giong error wrapped in an application's
  error type (a pool timeout inside a `RepoError`) was a `500` instead of a `503`, and an
  upstream's message relayed in `io::Error::other("GIONG-E1111: ...")` was a `413`. The
  code is now looked for down the whole `source()` chain, by type for giong's own errors
  and by message only for the companion crates' error types, never in an `io::Error`'s
  text.
- **giong-web**: `Compression` answered `HEAD` without `Content-Encoding: gzip` and with
  the uncompressed `Content-Length`, while `GET` got the compressed ones; RFC 9110 asks
  for the same header fields. `HEAD` is now compressed like `GET`, and the server sends
  the compressed length without the body.
- **giong-tls**: in the process, a TLS 1.2 AES-GCM connection whose key had sent 2^24
  records went on returning `Ok` from `write_all` while rustls dropped the data and sent
  `close_notify` in its place, so the peer saw a clean close and part of the data was
  lost. The write that meets the limit now fails with `GIONG-E2012`, as under kernel
  TLS, and so does every later one.
- **giong-tls**: in the process, two tasks writing at once could interleave their records
  on the wire (2 MiB from two writers arrived in four pieces). Each write now holds the
  send side until it is all sent, as under kernel TLS; a `flush` waiting behind a writer
  lets that writer carry its records.
- **giong-tls**: each stream kept its outgoing record buffer at its largest size for its
  whole life, about 64 KiB per connection after any write of 64 KiB or more. A buffer
  larger than 16 KiB is now dropped once sent: 50 connection pairs keep 29 KB each after
  a 1 MiB write, against 95 KB before (`tests/memory.rs`).
- **giong-tls**: kernel TLS fails the connection with `unexpected_message`, as rustls
  does, when the peer sends another record in the middle of a handshake message split
  over records (RFC 8446, 5.1), instead of accepting it.
- **giong-tls**: the handoff to kernel TLS tracks record boundaries from the bytes as
  they arrive, instead of reading record headers back from the buffer that rustls
  decrypts and joins in place. A peer that cut a message after its `Finished` over
  three or more records could make the handoff read once too often or fail with "record
  larger than TLS allows". Tested with a peer that cuts its records to 32 bytes.
- **giong-tls**: on Linux 6.14 and newer, a TLS 1.3 connection in the kernel could fail
  with `GIONG-E2011` ("the kernel waits for new TLS keys") when the peer split a
  handshake message over records, a session ticket for example. Those kernels take every
  handshake record whose first byte is 24 for a `KeyUpdate` and receive nothing more
  until they get keys, and a later piece of a split message starts with 24 once in 256
  times. giong-tls, which joins the messages, now gives the kernel its keys back at the
  sequence number it has reached when no `KeyUpdate` came, and the connection goes on.
  Found by the test with a peer that cuts its records to 32 bytes, which failed once in
  CI; a new test cuts a ticket so that it happens every time.
- **giong-tls**: the docs say that `Ktls::Auto` can still fail with `GIONG-E2010` if the
  kernel refuses keys after the handshake (rustls has handed them over by then), that
  only one task may receive at a time, and that `tcp()` is for addresses and socket
  options only.
- **giong-compat**: on the epoll driver, `poll_write` with a full buffer could return
  `Pending` after the send it waited on finished on that same poll, so no operation held
  the task's waker (hundreds of times in 2 GiB over a Unix socket). Only the epoll
  driver's own waker list woke the task again. A write now waits for the send in flight
  before starting the next one, so every `Pending` leaves a send holding the waker.
- **giong-compat**: on io_uring, an idle stream kept its last send buffer (up to the
  64 KiB write buffer, as the 0.5.0 README warned). It now drops the buffer when a send
  finishes, so an idle stream holds none again; `tests/memory.rs` checks that it holds
  less than 1 KiB on both drivers.
- **giong-compat**: a write made while a TLS shutdown is in progress fails at once with
  `GIONG-E1402` instead of being accepted and then dropped, and a shutdown that failed
  keeps failing (`GIONG-E1401`) instead of returning `Ok` the second time.
- **giong-compat**: `consume` with nothing buffered is ignored, as in std's and Tokio's
  `BufReader`, instead of panicking in debug builds.
- **giong-compat**: tests for what the review found untested: a seeded cancellation
  stress test (`tests/cancel.rs`), hyper's HTTP/2 client and server over `Compat` with a
  giong executor, and `Compat` over kernel TLS. `scripts/valgrind.sh` runs the `compat`
  and `memory` suites too.
- **giong-postgres**: session state made on a connection from `Pool::get` (`SET`,
  `SET ROLE`, temporary tables, advisory locks, `LISTEN`, open cursors, an SQL-level
  `PREPARE`) stayed on it when it went back to the pool, and reached the statements of
  other tasks and the next `get`. A connection that was used now comes back with its
  session reset (`SET SESSION AUTHORIZATION DEFAULT; RESET ALL; CLOSE ALL; UNLISTEN *;
  SELECT pg_advisory_unlock_all(); DISCARD TEMP; DISCARD SEQUENCES`, plus `DEALLOCATE ALL`
  after a `PREPARE`), pipelined ahead of the next statement, so nobody waits a round trip
  for it; settings given to `Config` survive it, and a connection whose reset fails is
  closed.
- **giong-postgres**: `Pool::get` could wait until it timed out while the shared
  statements of other tasks kept every connection busy. Tasks waiting in `get` are now
  served in the order they came, and shared statements stop using a connection while a
  `get` waits for it (all but one, when the pool has room for two). A pooled connection
  carries at most 1024 statements at a time; past that, the statement methods use another
  connection or wait for room.
- **giong-postgres**: connecting through PgBouncer failed, because it refuses the
  `statement_timeout` sent when connecting, and the README's advice for it
  (`statement_cache_capacity(0)`) was wrong. The error now says to call
  `Config::statement_timeout(None)` and set the limit with `ALTER ROLE` (PgBouncer's
  `track_extra_parameters` accepts the setting without applying it), and the README says
  that prepared statements through PgBouncer in transaction mode need PgBouncer 1.21 or
  later with `max_prepared_statements`. CI tests both through PgBouncer.
- **giong-postgres**: after a schema change under a cached statement, its next run failed
  once on each connection, and on a server whose messages are not in English
  (`lc_messages`) every run failed, for good. Stale statements are now recognised by
  SQLSTATE and the server's routine, prepared again and run once more, so the caller sees
  no error; inside a transaction, which the error has aborted, that one run still fails.
- **giong-postgres**: a notice or notification larger than `max_result_size` closed the
  connection with a message asking whether the server was really PostgreSQL. It is now
  skipped, and an error message that large fails only its statement, with
  `ErrorKind::TooLarge`.
- **giong-postgres**: compiles on systems other than Linux (`TCP_USER_TIMEOUT` is set on
  Linux only), ahead of the macOS milestone.
- **giong-postgres**: CI also runs the tests against PostgreSQL 18, and a new fuzz target,
  `session`, plays a server that answers a connection's requests with arbitrary bytes.
- **giong-mysql**: text could be stored in the wrong character set without an error. The
  character set was asked for only by the handshake's collation id (255,
  `utf8mb4_0900_ai_ci`), which a server that does not know it (MariaDB up to 11.4.4) or
  ignores it on purpose (MySQL with `skip-character-set-client-handshake`) replaces with
  its own default. With a latin1 default, UTF-8 was stored double-encoded: giong-mysql
  read its own text back unchanged, while every other client, `CHAR_LENGTH`, `LIKE` and
  sorting saw it corrupt. The session now runs `SET NAMES utf8mb4` and checks the
  character sets in the same round trip, refusing to connect when something such as a
  proxy has undone it (`ErrorKind::Connect`).
- **giong-mysql**: a transaction could commit part of its work. With
  `innodb_rollback_on_timeout` on, a lock wait timeout (error 1205) rolls the whole
  transaction back, and so does a full lock table (1206), but the client treated only a
  deadlock (1213) that way: the statements after it ran under autocommit, and `commit()`
  succeeded. The client now reads `innodb_rollback_on_timeout` when connecting and treats
  1205 like 1213 when it is on, and 1206 always. After any other error in a transaction
  (a Galera conflict, say), it asks the server whether the transaction is still open (a
  `COM_PING`, whose answer carries the status flags) before sending anything else. A
  transaction the server ended refuses its next statements and its `commit()`.
- **giong-mysql**: a statement given up inside a transaction (its future dropped, by a
  timeout say) still ran before `COMMIT`, so `commit()` committed work the application
  believed undone. The transaction is now marked, and its next statements and `commit()`
  fail; dropping it rolls it back.
- **giong-mysql**: a result announcing more columns than a statement can have (65,535) is
  refused (`ErrorKind::Protocol`); a hostile server could make the client keep column
  definitions without end. One large row or statement no longer leaves the
  connection holding buffers its size: past 1 MiB they are given back once idle.
- **giong-mysql**: a server error message that is not UTF-8 is read with replacement
  characters instead of closing the connection. `execute()` on a `SELECT` with a row
  over `max_result_size` no longer fails with `TooLarge`, since it keeps no rows.
- **giong-mysql**: an RSA public key with an exponent below 3 is refused. The
  `TCP_USER_TIMEOUT` setting is compiled on Linux and Android only, and the `_os`
  connection attribute names the system it runs on.
- **giong-mysql**: `insert()`'s error when no id was generated says that the statement ran
  and must not be run again, and its documentation says so.
- **giong-redis**: a `ToArg` that called its callback twice (a typed key written as a
  prefix, then an id) or not at all made the command methods (`get`, `set`, `hget`...)
  send one argument more or fewer than the count in front of them. Redis ran the extra
  one as a command of its own, and from then on every task sharing the connection got the
  reply of the command before its own (`GET` of one key returned another key's value)
  until the connection failed. With the new `ToArg` this cannot happen. A `ToArg` that
  panics no longer leaves half a command in the connection's buffer, where the next task's
  command was appended to it.
- **giong-redis**: `Pool::take` failed with `PoolTimeout` whenever the pool was full and
  shared commands kept every connection busy: with 32 tasks sending `GET`s and `max_size`
  1 or 2, after the whole wait, every time. Tasks waiting in `take` are now served in
  order, and a connection is set aside for the first of them: shared commands go to the
  others, and it is handed over once the commands already on it are answered (in
  milliseconds in the same test). Shared commands keep at least one connection unless
  `max_size` is 1.
- **giong-redis**: a pool kept its connections to a server that had become a replica (the
  old primary after a failover keeps them open and answers every write with `READONLY`)
  until they dropped for some other reason. A connection whose server answers `READONLY`
  is now left behind, and closes once the commands on it are answered, so the next
  command opens a new one; `Error::is_transient` is true for `READONLY`.
- **giong-redis**: `Pool::wait_timeout(Duration::MAX)` made every command panic (an
  overflow adding it to the clock); it now waits as long as it takes.
- **giong-redis**: `PubSub::subscribe`, `psubscribe`, `unsubscribe` and `punsubscribe` took
  the first confirmation of their kind for their own, even one for another channel left
  by a call that was cancelled; they now wait for the one for their channel.
- **giong-redis**: a push message too large to keep, behind an attribute that had not
  arrived whole when the message went over the limit, was taken for the reply to the
  oldest command, and every later reply went to the command after its own. Whether a
  reply is a push message is now decided when its own type is read, behind any
  attributes. Redis and Valkey do not put attributes in front of push messages.
- **giong-redis**: a dropped `Client` kept its read task and its socket until the server
  closed its side, which a proxy that does not pass a half-close on may never do. It now
  closes once the replies to the commands already sent have come, or after the response
  timeout.
- **giong-redis**: a URL whose password holds an unescaped `/` or `?` failed with a
  message quoting a piece of the password as a port, a path or a parameter (`the port
  "pa" in the URL is not a number`); it now says to percent-encode them, and quotes
  nothing.
- **giong-mcp**: "over the last second" figures (`busy`, `polls_per_s`, ...) covered as
  little as 2 ms after an agent called `overview` about a dozen times in a row, since the
  calls' samples pushed the sampler's out of a history kept by count; it is kept by time
  now, and rates need 100 ms of history. A panicking registered source is reported as
  such, with its message, instead of as a stopped worker. Integer arguments sent as `2.0`
  are accepted, as JSON Schema allows. A message with a non-string `method`, or with an
  `id` and neither `method` nor `result`/`error`, gets `-32600` instead of a silent `202`.
- **giong-mcp**: a Unix socket is created with mode `0600` whatever the umask (it was
  `0775` under `umask 002`), bound under a temporary name and linked into place so that it
  is never reachable with looser permissions and never replaces a live socket. Stopping
  removes only the socket this server created, not one a later process bound at the same
  path.
- **giong-mcp**: on Linux 6.0.19 a server asked for a TCP address that was in use started
  anyway, on a free port of every interface: that kernel's `bind` returns 1 instead of an
  error and leaves the socket unbound, which the standard library and socket2 both read
  as success, and listening then picks a port on `0.0.0.0`. The server now checks that it
  listens where it was asked and otherwise fails with `GIONG-E1303`. Found by the 6.0.19
  job in CI; later kernels return the error.

### Security

- **giong-mysql**: a connection taken with `Pool::get()` came back to the shared set with
  its session as the task left it, and every task then ran in that session: a `SET`
  (`sql_mode`, the isolation level, `foreign_key_checks = 0`), a temporary table that
  hid a real one, user variables, `autocommit = 0`, and locks (`LOCK TABLES`,
  `GET_LOCK`) that stayed held until the connection closed. Such a connection is now
  reset when it comes back: `COM_RESET_CONNECTION`, the session settings of `Config`
  again and, when `Config::database` is set, `COM_INIT_DB`, all queued at once with no
  round trip waited for. The reset frees the connection's prepared statements, so it is
  skipped when the connection ran only plain statements (`SELECT`, `INSERT`, `UPDATE`,
  `DELETE`... with no user variable or named lock) and the server's session tracker
  reported no change. Without `Config::database`, a connection whose default database a
  `USE` changed is closed instead, since the reset cannot undo it. A reset that fails
  closes the connection. The server does not report a named lock or a user variable set
  inside a stored function or trigger; the new `Client::reset_session` resets by hand.
  Found in the pre-1.0 review.
- **giong-mysql**: a `CancelToken` opened a new connection to the configured host name,
  which behind DNS round robin or a load balancer could reach another server, and sent
  `KILL QUERY` with a connection id that there belongs to some other session: a
  round-robin proxy in the test stopped another client's statement. The cancel request
  now connects to the address the connection reached (checking the certificate against
  the configured name) and sends `KILL QUERY` only if the server there is the same one
  (`@@server_uuid` on MySQL; host name, port and `server_id` on MariaDB); otherwise it
  fails with `ErrorKind::Connect` and kills nothing. As before, what is stopped is
  whatever the connection runs when the request arrives, which may be the next statement
  in line; the documentation says so.
- **giong-mcp**: any local client could make the service's process run out of memory: one
  254 KiB `POST` holding a JSON-RPC batch of 3,275 `workers` calls was answered with
  14 MB built in memory, and 24 such connections took a 4-worker service from 4 MB to
  2 GB of resident memory (1.9 GB still held afterwards). Now the server speaks HTTP/1.1
  only (an HTTP/2 connection could carry a hundred requests at once) on at most 16
  connections, bodies hold at most 64 KiB, batches at most 16 messages and only for
  clients of MCP 2025-03-26 (later versions removed them), and an answer is at most
  4 MiB, past which the agent is told to ask for less. 64 such connections leave the
  service under 8 MB.

## [0.5.0] - 2026-11-14

HTTP/2, and less work per byte. `giong-http` speaks HTTP/2 as a server and as a client,
through ALPN or with prior knowledge, with the same handlers; it is hardened against rapid
reset, floods and HPACK bombs, `h2spec` passes all 147 cases, and on one core of a
bare-metal server it answers 2.3 to 4.3 times as many HTTP/2 requests as axum. `giong-tls`
can hand a connection to the kernel's TLS after the handshake, and files go out with
`sendfile`: on bare metal a 1 MiB file over HTTPS is served 36% faster than with rustls,
level with nginx. On io_uring, large static bodies leave without a copy on Linux 6.2 and
newer (up to 85% less CPU per request), connections share receive buffers piece by piece
on 6.12 and newer (less than half the memory under load), and bulk receives are no longer
copied out of the pool. `SQPOLL` was tried and left out: on bare metal it served fewer
requests with 73% more CPU per request. The new `giong-compat` gives giong's streams the
Tokio and futures-io I/O traits, so crates such as h2, tokio-rustls and tokio-tungstenite
run on giong. On Linux 6.14 to 7.2, a worker past the locked-memory limit now runs on
epoll and says why (`GIONG-E0010`) instead of stopping the runtime. No change breaks code
written for 0.4, but a stream's `recv` may now return one write of the peer in two pieces.

The release candidate was measured again on the two bare-metal machines
(`bench/results/bare-metal-epyc-4345p/linux-7.0/release-0.5.0/`): on 4 workers giong
served 1.75M requests/s against 1.69M for tuned Monoio and 1.59M for raw Tokio, two
15-minute soaks passed, and nothing moved beyond the 2% noise between runs except two
small losses not explained yet: an idle HTTP/2 connection costs 4.21 KiB where an earlier
build measured 3.99, and giong-http over TLS answers about 2% fewer small requests.

### Added

- **`giong-http`: HTTP/2** (RFC 9113) for the same handlers. Over TLS, `serve_tls` offers
  `h2` and `http/1.1` through ALPN, so browsers get HTTP/2; without TLS, a connection that
  opens with the HTTP/2 preface is served over HTTP/2 ("prior knowledge"), on the same
  port as HTTP/1.1. Each stream is a `Request` like any other, so handlers and `giong-web`
  apps serve HTTP/2 unchanged; `Request::version()` returns the new `Version` (`Http10`,
  `Http11`, `Http2`). Flow control both ways, HPACK with its dynamic table, streamed
  bodies both ways, file bodies (`Body::File`, read in 64 KiB pieces into DATA frames; a
  file shorter than announced resets its stream), `Expect: 100-continue`, trailers, and
  graceful shutdown with a GOAWAY after which the streams in progress finish. A stream
  the client resets drops its handler at once. `Server::http2(false)` serves HTTP/1.1
  only, and `Server::max_concurrent_streams` (default 100) caps the streams of one
  connection. `h2spec` passes all 147 cases over TLS in strict mode, and runs in CI.
- **`giong-http`**: every resource an HTTP/2 client can make the server hold is bounded,
  and a client that overdraws a limit gets a GOAWAY whose debug data carries a code with
  a page in `docs/errors`: `GIONG-E1115` (protocol error), `GIONG-E1116` (rapid reset:
  too many streams reset or refused), `GIONG-E1117` (a flood of frames that do no work:
  PING, SETTINGS, PRIORITY, empty DATA or CONTINUATION, needless WINDOW_UPDATE) and
  `GIONG-E1118` (a header block larger than `max_head_size`, the CONTINUATION flood). A
  header list that decodes larger than `max_head_size` (an HPACK bomb) is answered with
  `431`. Request bodies are received only as fast as the handler takes them, and the
  bodies one connection holds share a budget (the larger of `max_body_size` and 1 MiB)
  instead of growing with the number of streams; a client that stops reading stops
  being read from, and a stream its flow control keeps waiting longer than
  `write_timeout` is reset.
- **`giong-http`**: on one core, HTTP/2 serves 3.8 to 4.4 times the requests per second
  of axum 0.8 (hyper) on small answers and 2.5 to 3.0 times on 8.5 KiB ones, over HTTPS
  and with prior knowledge (502k against 131k for a 27-byte answer over HTTPS). An idle
  HTTP/2 connection costs 4.2 KiB (axum 23.8), one with a request in progress 5.9 KiB
  (23.0), and each further request 1.1 KiB (2.0); an idle HTTP/1.1 connection still
  costs 1.9 KiB. `bench/h2.sh` measures both; results are in `bench/results/h2/`.
- **`giong-http`: HTTP/2 in the client.** `https` connections offer `h2` and `http/1.1`
  through ALPN, and a server that picks `h2` gets every request to its origin on one
  connection, as many at once as it allows (another connection is opened, up to
  `max_connections_per_host`, when it allows no more). `ClientBuilder::http2(false)`
  sticks to HTTP/1.1; `ClientBuilder::http2_prior_knowledge(true)` speaks HTTP/2 to
  `http` URLs (gRPC backends, giong-http's own server). `Response::version()` and
  `StreamingResponse::version()` say which was spoken. What a server can make the client
  hold is bounded by the windows it grants (1 MiB of each response body, 4 MiB on a
  connection, granted again only as bodies are read). A request the server refused
  unprocessed (`REFUSED_STREAM`, or beyond the last stream of a GOAWAY) is sent again
  whatever its method; a dropped request or streamed response resets its stream, and
  the connection carries on. Redirects, decompression, streamed bodies, timeouts and the
  limits work as over HTTP/1.1.
- **`giong-http`**: over HTTP/2, the client on one core sends 2.0 to 2.2 times the
  requests per second of reqwest 0.12 on small answers (251k against 123k over HTTPS)
  and 1.2 to 1.3 times on 8.5 KiB ones. `bench/http-client.sh` measures HTTP/2 over TLS
  and with prior knowledge (`h2`, `h2c`), and tells both clients to speak HTTP/1.1 for
  `http` and `https`.
- **Kernel TLS in `giong-tls`**: `TlsAcceptor::ktls(mode)` and `TlsConnector::ktls(mode)`
  hand a connection's keys to the kernel once rustls has done the handshake, and the
  kernel encrypts and decrypts from then on; the `TlsStream` API stays the same.
  `Ktls::Off` is the default; `Ktls::Auto` uses the kernel where it can do everything
  rustls would have done (the `tls` module, every cipher suite the configuration
  allows in both directions, and new TLS 1.3 keys on a live connection, Linux 6.14+)
  and keeps the connection in the process otherwise; `Ktls::Required` fails with
  `GIONG-E2009` instead. What the kernel can do is probed once per process
  (`giong_tls::ktls_support()`, one line for a log); `TlsStream::is_ktls()`,
  `ktls_unavailable()` and `giong_tls::ktls_stats()` say which way connections went.
  Session tickets, key updates from either side, alerts, refused renegotiation and
  `close_notify` work as with rustls in the process. The TLS 1.2 AES-GCM handoff starts
  the kernel's explicit nonces past every nonce rustls used: handed rustls' IV as it is,
  the kernel would repeat the nonce of the `Finished` message under the same key. New
  error codes `GIONG-E2009` to `GIONG-E2012` explain each failure. Linux 6.0's kernel
  TLS is refused (it can crash the kernel when a connection closes); 5.15 and 6.1 on
  are tested.
- **Files over TLS without copying them**: `giong::net::TcpStream::send_file(&file,
  offset, len)` sends a range of a file with `sendfile(2)`, straight from the page cache,
  on both drivers; with kernel TLS the kernel encrypts the file as it sends it.
  `giong_tls::TlsStream::send_file` does that under kernel TLS and reads and encrypts in
  256 KiB pieces in the process otherwise. `giong-http` sends the new `Body::file(file)`
  and `Body::file_range(file, offset, len)` that way (`Transport::write_all_with_file`;
  Unix sockets read and write). A file that turns out shorter than announced is the new
  error `GIONG-E0201` (`Code::FileTooShort`), after what it had was sent. On bare metal
  (EPYC 4345P, Linux 7.0, one server core) giong-http serves a 1 MiB file over HTTPS at
  6,104 req/s with kernel TLS and `sendfile` against 4,484 with rustls (+36%; nginx with
  kernel TLS and `sendfile`: 6,039). With 64 KiB files, from memory and for small
  responses kernel TLS is 4–22% slower than rustls, so `Ktls::Off` stays the default; the
  `giong-tls` README has the table and when to turn it on. Recorded in
  `bench/results/bare-metal-epyc-4345p/linux-7.0/ktls/`.
- **`giong-tls`**: `TlsStream::write_all_vectored(bufs)` sends several buffers, a header
  and a body for example, without copying them into one first. `giong-http` uses it for
  bodies of 32 KiB or more over HTTPS, which it used to copy after the head.
- **giong**: on Linux 6.2 and newer, TCP sends of 16 KiB and more from bytes that cannot
  change (`&'static [u8]`, `Rc<[u8]>`, `Arc<[u8]>`, so `giong-http`'s static bodies) go
  out without a copy into the kernel (`IORING_OP_SEND_ZC`, `zero-copy-send`): the network
  card reads them, and the write still returns as soon as they are queued, so a writer is
  never held to the peer's acknowledgements. A vectored write may add up to 4 KiB from
  other buffers (an HTTP head), which are copied aside. Bytes that can change once the
  write returns (a `Vec`) are copied as before. A socket goes back to plain sends for good
  when the kernel reports that it copied the data anyway (loopback, a card without
  scatter-gather) or cannot send this way (kTLS); Unix sockets never use it. On bare metal
  (Linux 7.0, 25 GbE) it cuts the CPU time per request by 16% at 64 KiB, 62% at 256 KiB,
  68 to 72% at 1 MiB and 85 to 86% at 16 MiB across runs, at the same request rate.
  `bench/uring-features.sh` measures each optional io_uring feature against the rest, on
  one machine or two.
- **giong**: `DriverChoice::io_uring_features` lists the optional io_uring features a
  worker uses (`giong::IoUringFeature`), and its one-line form names them
  (`io_uring (chosen automatically), using packed-buffers`). Each is probed when a worker
  starts and skipped where the kernel lacks it. `GIONG_IO_URING_OFF` (names, or `all`)
  turns them off for tests and benchmarks, as `GIONG_RING_TIER` does for the ring setup.
- **giong**: `TcpStream::poll_recv` and `UnixStream::poll_recv`, the poll form of `recv`,
  for futures written by hand and for adapters such as giong-compat.
- **giong**: `TcpStream::send_with_control(buf, control)`, one `sendmsg` with ancillary
  data, on both drivers (giong-tls sends TLS alerts and key updates through it).
- **giong-compat**, a new crate: `Compat` gives giong's `TcpStream`, `UnixStream` and (with
  the `tls` feature) giong-tls's `TlsStream` the `AsyncRead`, `AsyncWrite` and
  `AsyncBufRead` traits of `tokio::io` and of `futures_io`, so that crates generic over
  their transport run on giong with no Tokio runtime: tested with tokio-rustls,
  futures-rustls, tokio-tungstenite, h2 (also over giong-tls with ALPN), hyper's HTTP/1
  connection API (against giong-http both ways) and tokio-util's codecs, on io_uring and
  epoll. Writes are buffered like `BufWriter` (64 KiB by default) and sent on `flush`, when
  the buffer is full or when a read waits; a partly read chunk never holds the worker's
  receive pool; `TCP_NODELAY` is turned on. It costs a copy each way (none for reads
  through `AsyncBufRead`), measured by `bench/compat.sh` in four sessions (sending 10–20%
  slower, receiving with `read` 6–13% more CPU per byte). Errors `GIONG-E1401` (an earlier
  buffered write failed), `E1402` (write after shutdown) and `E1403` (invalid setting). A
  hyper server over `Compat` needs a `hyper::rt::Timer` for its header timeout; the README
  shows one built on giong's timers.
- **Verification**: two new fuzz targets in the nightly fuzz workflow, `hpack` (header
  blocks decoded, encoded again and compared; Huffman both ways) and `h2` (whole
  connections from a client that follows the protocol mostly, every frame the server
  writes checked against RFC 9113 and the client's flow control). The same checks run
  in `cargo test` (100,000 header blocks and 3,000 connections a run).
- **Verification**: a fuzz target for the client's side of HTTP/2, `h2_client`: frames a
  server might send, fed in pieces while requests come and go, with checks that the
  client holds no more than the windows it grants and writes only well-formed frames
  within the server's windows. 3,000 such connections run in `cargo test`.
- **Verification**: `KTLS=1 scripts/kernels.sh 6.1.155 6.18.63` loads the kernel's `tls`
  module in the QEMU guest (from Ubuntu's modules package) and requires the kernel TLS
  tests to run there (`GIONG_KTLS_REQUIRED=1`); CI does this on 5.15 (epoll), 6.1,
  Ubuntu's 6.8 and 6.18, and loads the module on its runners for the `test`,
  `fallback`, `tsan` and `docker` jobs.
- **Verification**: `scripts/check.sh fallback` (and CI's `fallback` job) also runs the
  suite with the optional features off, and the QEMU kernels include 6.12, the first with
  packed buffer rings; every QEMU run checks which features giong found.
- **Benchmarks**: `bench/tls.sh` adds `giong-http-ktls` (giong-http with
  `Ktls::Required`), 64 KiB and 1 MiB responses from memory (`bytes64k`, `bytes1m`) and
  from a file (`file64k`, `file1m`, `file64k-open`), the server's CPU time per request
  and an error count; `SERVERS` and `ENDPOINTS` pick a subset. `bench/http-client.sh`
  adds a giong client with kernel TLS and the same body sizes.
- **Docs**: an addendum to the `giong-tls` security review covers kernel TLS
  (`docs/reviews/2026-11-giong-tls.md`).

### Changed

- **giong**: on Linux 5.18 and newer, a worker enters io_uring through its thread's
  registered ring table (`IORING_REGISTER_RING_FDS`) instead of the ring's descriptor, so
  the kernel no longer looks the descriptor up on each `io_uring_enter` (about 11 of the
  130 ns an enter costs). `DriverChoice` lists it as `registered-ring`.
- **giong**: on Linux 6.12 and newer, stream receives fill the worker's pool buffers piece
  by piece (a buffer ring with `IOU_PBUF_RING_INC`) instead of taking a whole 8 KiB buffer
  each, so connections that each send a little share a few buffers. With 20 workers and
  5,000 busy connections, giong-http's peak memory fell from 50 to 23 MiB (epoll: 15 MiB);
  requests per second did not change. Datagrams still take whole buffers. A stream's data
  can now be cut where a buffer fills, so one write by the peer may come back from `recv`
  in two pieces; streams never promised otherwise.
- **giong**: a stream that receives a lot at once (a bulk transfer, a large upload) is no
  longer copied out of the pool on io_uring. More than 8 receives for one stream between
  two turns of the event loop used to count as a reader falling behind: the rest was
  copied to the heap and the receive paused, which on a 256 KiB-at-a-time transfer copied
  9 bytes in 10. The limit is now an eighth of the pool (64 receives by default), and
  only data its reader left unread for a whole turn is copied, while the pool runs low or
  is empty. Receiving such a transfer on loopback now costs the worker 167 instead of
  565 ns of CPU per KiB (epoll: 169), at 3.0 instead of 1.6 GiB/s. On kernels before 6.1
  (no `DEFER_TASKRUN`), where one fast stream can fill the whole pool in a single turn,
  the cost falls by a third to a half (measured on 6.18 with the ring setups giong uses
  on 5.19 and 6.0, then on 5.15: 171 instead of 253 ns per KiB, 184 instead of 338).

### Fixed

- **giong**: Linux 6.14 to 7.2 count every io_uring ring against the locked-memory limit
  (`RLIMIT_MEMLOCK`) of a process without `CAP_IPC_LOCK`, summed over the user's processes
  (7.3 stopped, and so did 6.18 from 6.18.58): about 150 KiB per giong worker, so the
  common 8 MiB limit holds about fifty workers. Past it, a worker whose rings did not fit
  failed with a bare `Cannot allocate memory`, which stopped the runtime, and one whose
  buffer ring did not fit fell back to epoll blaming the kernel (`GIONG-E0009`, "may be a
  kernel bug"). Such a worker now runs on epoll, unless io_uring was asked for, and the
  error is the new `GIONG-E0010` (`Code::LockedMemoryLimit`), which names the limit and
  how to raise it; [DEPLOYMENT.md](docs/DEPLOYMENT.md#locked-memory-limit) has the
  settings for systemd, Docker and Kubernetes. CI's runners have 7.0 kernels and a limit,
  which a test running several runtimes at once went past; CI now lifts the limit there,
  so that the suite runs on io_uring every time, and a test sets it to nothing to check
  the fallback.
- **giong**: on Linux 6.0 and 6.1, io_uring gives up waiting on a non-blocking socket
  when a woken `sendmsg` or `recvmsg` still cannot proceed (it retries on a kernel
  thread, which then sees `O_NONBLOCK`) and completes with `EAGAIN`. giong now submits
  such an operation again instead of returning `WouldBlock`. Sockets were blocking under
  io_uring until `send_file`, which needs `O_NONBLOCK`; a test with four writers on one
  small buffer failed on both kernels without the fix.
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

[Unreleased]: https://github.com/giong-run/Giong/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/giong-run/Giong/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/giong-run/Giong/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/giong-run/Giong/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/giong-run/Giong/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/giong-run/Giong/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/giong-run/Giong/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/giong-run/Giong/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/giong-run/Giong/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/giong-run/Giong/releases/tag/v0.1.0
