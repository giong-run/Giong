# Stability

From 1.0.0 on, giong keeps major version 1 for a long time and grows through 1.x minor
releases. Code that builds and behaves correctly on 1.N builds and behaves the same way on
every later 1.x. This page says exactly what that promise covers, what it does not, and how
the parts that have to move (the Rust version, the Linux kernel, the crates giong depends
on) move without breaking it.

`scripts/semver.sh` (cargo-semver-checks, run on every pull request) enforces the Rust API
part; the rest is kept by review.

## What the promise covers

All nine published crates: `giong`, `giong-http`, `giong-tls`, `giong-postgres`,
`giong-mysql`, `giong-redis`, `giong-web`, `giong-mcp` and `giong-compat`.

- **The Rust API**: every public item that shows in the documentation, with its
  signature, its trait implementations and its auto traits. Whether a type is `Send` or
  `Sync` is part of the API: a type that is `Send` today stays `Send`. A type that is not
  may become so in a minor release (adding an auto trait breaks nothing), as `JoinError`,
  `RecvBuf` or a database `Row` might.
- **Error codes.** A `GIONG-Exxxx` code keeps its number, its meaning and its
  `io::ErrorKind` forever, and is never reused for something else. A code that is no longer
  raised keeps its page under `docs/errors/`. New codes are added in minor releases, which
  is why `giong::error::Code` is `#[non_exhaustive]`. The message text and the fix text
  around a code may be improved in any release: match on the code, not on the text.
- **Environment variables** that change behaviour: `GIONG_DRIVER`, `GIONG_MCP` and
  `GIONG_MCP_TOKEN`. Their accepted values keep working; new values may be added.
- **Machine-read output**: the JSON lines that `giong_web::middleware::Logger` writes and
  the tools and result fields that `giong-mcp` serves keep their names and meaning. Fields
  may be added, and a reader must ignore fields it does not know.
- **Documented behaviour**: limits and defaults that the documentation states, such as
  "every limit has a finite default" or "dropping an I/O future never frees memory the
  kernel may still write to". A default value may be tuned in a minor release when the
  documentation does not promise the number; the change is in the CHANGELOG.

## What it does not cover

- **Items marked `#[doc(hidden)]`**, such as `TcpStream::send_with_control` (which
  `giong-tls` uses for kernel TLS) and the `fuzzing` modules built only under
  `--cfg fuzzing`. They are public only so that the crates of this workspace can reach
  each other; they may change or go in any release. cargo-semver-checks ignores them too.
- **Features named `unstable-*`**, such as `giong-tls`'s `unstable-rustls`, which exposes
  rustls' own types. Their items follow the version of the dependency giong uses and may
  change in a minor release; the CHANGELOG says so when they do.
- **`Debug` output**, the wording of error messages and log lines meant for people, and
  the exact numbers in the `observe` statistics (their fields stay; what one unit of them
  costs may change).
- **Performance.** Speed and memory are measured in CI (`bench/instr`) and only get better
  on purpose, but a number is not an API.
- **Which driver a worker gets.** A newer kernel may get a worker the io_uring driver where
  an older giong chose epoll, or the reverse when a kernel bug needs avoiding
  (`KernelWorkaround`); both drivers behave the same through the API.

## How the API grows

These rules let 1.x add what the roadmap needs (gRPC, macOS, HTTP/3, Windows) without a
2.0:

- **Enums and structs with public fields are `#[non_exhaustive]`**, so variants and fields
  can be added. The few exhaustive enums are complete by nature (`mpsc::TryRecvError`,
  `oneshot::TryRecvError`, `TrySendError`, `giong_postgres::types::Format` and `Encoded`)
  and stay as they are.
- **Traits that users implement** (`IoBuf`, `IoBufMut`, `observe::Source`,
  `giong_http::Handler`, `giong_http::Transport`, `giong_web::Middleware`, the database
  crates' `ToSql`, `FromSql`, `FromValue` and `ToArg`) only gain methods that have a
  default. Traits users are not meant to implement are sealed (`ToSocketAddrs`,
  `RowIndex`, `giong_compat::Transport`); a sealed trait may be opened later, never the
  reverse.
- **New ways of doing something come as new items**: a method, a constructor, a builder
  setting, a type, a crate. An existing signature does not change. A name that turns out
  wrong gets a new name and the old one stays as a deprecated alias for the rest of 1.x.
- **New platforms and protocols are new code behind the same signatures.** The epoll
  driver already runs the whole API on a readiness-based kernel interface, as kqueue on
  macOS is; IOCP on Windows is completion-based, as io_uring is, which the owned-buffer
  API (`IoBuf`, `RecvBuf`) was built for. Methods that only make sense on Unix
  (`OpenOptions::mode`, `Metadata::mode`, `DirEntry::ino`, the `AsRawFd` implementations)
  stay on Unix when other systems are added. HTTP/3 comes as a new entry point next to
  `Server::serve_tls`, and `giong_http::Version` can take a new variant.

## Public dependencies

A type from another crate in giong's API ties giong's major version to that crate's. The
stable API uses only these, all at a major version they have promised to keep:

| Crate | Where | Version |
|---|---|---|
| `tokio` (the I/O traits only, no runtime) | `giong-compat`, feature `tokio` | 1.x |
| `futures-io` (the I/O traits) | `giong-compat`, feature `futures-io` | 0.3 |
| `rustls-pki-types` (`CertificateDer`) | `giong-tls` | 1.x |
| `serde` (`Serialize`, `Deserialize` bounds) | `giong-web`, feature `serde` | 1.x |

If one of them ever publishes a new major version, giong adds support for it under a new
feature and keeps the old one; nothing is taken away in 1.x. Every other dependency
(`io-uring`, `libc`, `socket2`, `rustls`, `ring`, `miniz_oxide`, `serde_json`, `tracing`,
`webpki-roots`) is private: giong can move to its next version in a minor release without
anyone noticing. rustls in particular is kept out of `giong-tls`'s stable API for this
reason; its types are reachable only through the `unstable-rustls` feature.

## Rust version (MSRV)

The libraries build with **Rust 1.80** and newer, checked by the `msrv` CI job.

- The minimum is raised only in a minor release, never in a patch release, and the
  CHANGELOG entry says so.
- It is raised to a Rust release that is at least six months old at the time, and only
  when that brings something giong needs (async closures, a newer `std` API, a fix).
- The edition of giong's own code may change in a minor release; it does not affect code
  that uses giong.

## Linux kernel

The io_uring driver needs **Linux 6.0** or newer; older kernels, tested down to **5.15**,
run on the epoll driver with the same API.

- These minimums are raised only in a minor release, announced in the CHANGELOG of the
  minor release before, and only for a kernel line that no longer gets upstream long-term
  support.
- Newer kernel features (zero-copy receive, newer io_uring operations) are used when the
  running kernel has them and give way to the older path when it does not; they never
  raise the minimum.

## Versions of the crates

The nine crates are released together and carry the same version. A crate that depends on
another depends on the same minor or newer. New crates (`giong-quic`, `giong-grpc` and the
others on the roadmap) join at the current version when they are first published.

## Cancellation

A future is cancelled by dropping it: a `timeout`, a `select` that takes another branch, a
task that is aborted or a runtime that shuts down. What every I/O operation does then is
part of the promise, because callers rely on it as much as on a return value.

- **The operation may still happen.** Dropping the future stops the caller from learning
  the result, not the kernel from acting. A write, a send, a rename or an `unlink` handed to
  the kernel (or, on the epoll driver, to a blocking thread) may complete after the drop,
  in full or in part. Each public async fn says in its documentation whether it is *cancel
  safe* (dropping it loses nothing and changes nothing the caller cannot see) and, when it
  is not, what may have happened; `tests/cancel_docs.rs` fails a public async fn that does
  not say.
- **The driver holds what the kernel may still touch.** The buffers an operation was given
  (`IoBuf`, `IoBufMut`, `Vec`), the path it reads and the structures it points at move to
  the worker's driver when the future is dropped. They are freed, or a pool buffer goes
  back to its pool, only once nothing can touch them any more:
  - on io_uring, when the operation's completion arrives (the driver asks the kernel to
    cancel it at once, and the completion comes either way); for a zero-copy send, when
    the kernel's notification says it stopped reading the buffer, which comes after the
    result;
  - on epoll, at once for an operation that was waiting for readiness (nothing was handed
    to the kernel), and when the blocking thread returns for a file operation it had
    already started; one it had not started is skipped.
  The caller never gets those buffers back: a dropped `write(buf)` loses `buf`. Until they
  are freed they count wherever they were counted before the drop (a pool buffer stays out
  of the pool; a request's bytes stay in `max_buffered_request_bytes`), and that moment is
  also the earliest at which any 1.x budget gives their share back.
- **Descriptors are never leaked or closed twice.** An `accept`, `connect` or `open` that
  completes after its future was dropped has its new descriptor closed by the driver. A
  descriptor that an owner (`File`, `TcpStream`, `UdpSocket`) closes is closed exactly
  once, and its number may be reused by the process only after that close. A close itself
  is never cancelled: once `File::close` is called, the descriptor ends up closed whether
  or not its future is awaited; only the result can be lost.
- **Slots are given back when the operation ends, not when the future is dropped.** A
  database statement that was sent keeps its place in the connection's pipeline until its
  answer has been read and thrown away (`Client::in_flight` counts it until then). An
  HTTP/2 client stream whose response future was dropped is reset with `CANCEL`.
- **Data received is not lost by a cancelled receive** where the documentation says
  *cancel safe*: `TcpStream::recv`, `UdpSocket::recv_from` and the channel receivers keep
  what arrived for the next call. `read` into a caller's buffer is not: bytes the kernel
  had copied in are lost with the buffer.
- **A transaction never commits work whose outcome it does not know.** A statement of a
  transaction that was sent and then given up (its future dropped before the answer) may
  have run, so the transaction refuses `commit` with a coded error and has to be rolled
  back (dropping it does that). giong-mysql does this today (`Tx::Abandoned`). *Known gap
  on `main`, fixed before 1.0.0-rc.1: giong-postgres's `commit` still commits.*

## Defaults you can rely on

When a limit is reached, a giong API does one of three things by default, and which one is
part of the promise: it **waits** (with no end, or until a stated timeout), or it
**refuses** with a coded error. A 1.x release never turns a default that waits into one
that refuses, or the reverse; a new way of refusing early (an overload policy, a per-tenant
budget) comes as a setting that is off until it is turned on. The numbers may be tuned when
the documentation does not promise them; the kind of answer stays.

| Limit | Default answer |
|---|---|
| A bounded channel is full (`send`) | waits until there is room |
| A database pool has no free connection (`Pool::get`) | waits up to `wait_timeout` (5 s), then `E3009`, `E3109` or `E3209` |
| The HTTP client has `max_connections_per_host` (16) open to the origin | waits for a connection to be free, within the request's `timeout` (30 s) |
| The HTTP server holds `max_connections` (10,000 per worker) | stops accepting; new connections wait in the kernel's backlog |
| A request would take a worker past `max_buffered_request_bytes` | refuses: `503` with `E1120` (HTTP/2: `REFUSED_STREAM`) |
| Free memory is below `min_free_memory` when a connection arrives | refuses: `503` with `E1121`, or a TLS connection closed before its handshake |
| An HTTP/2 client opens more than `max_concurrent_streams` (100) | refuses the extra streams, as HTTP/2 requires |

The two memory refusals are on by default because waiting is not possible there: the
memory is already gone. Everything else waits, so code written for 1.0 sees no new errors
from a 1.x release unless it turns a new policy on.

## Changes made before 1.0

Breaking changes are refused between releases (`scripts/semver.sh`), so the few the API
review left for 1.0 are made together by one release pull request, the first after the
0.9 soak (0.9.0 or 1.0.0-rc.1, see [RELEASING.md](RELEASING.md)). Nothing uses giong in
production yet, so no deprecated alias is kept: old names go outright. This list is that
pull request's checklist:

- every item deprecated during 0.9 is removed, each listed under *Removed* with the name
  that replaces it: `Error::code` in giong-postgres and giong-mysql (`server_code`),
  `Config::dbname` in giong-postgres (`database`), `giong::sync::mpsc::channel`
  (`unbounded`), `TlsAcceptor::new` and `TlsConnector::new` (`from_rustls`, or the stable
  constructors), `TlsConnector::with_roots` (`with_root_pem`);
- `giong::sync::mpsc::Sender` and `Receiver` go to the bounded channel, and
  `BoundedSender` and `BoundedReceiver` are removed (the API freeze record,
  [reviews/2026-11-api-freeze.md](reviews/2026-11-api-freeze.md)); the unbounded halves
  stay `UnboundedSender` and `UnboundedReceiver`;
- the tests of the old names go with them, and the README's test counts drop to match;
- the texts that describe keeping aliases (the freeze record, RELEASING.md, the
  *Deprecated* section of CHANGELOG.md) say what was done instead;
- `giong_mcp::FromEnv` becomes `#[non_exhaustive]`, like every other enum that is not
  complete by nature.
