# AGENTS.md

Instructions for AI coding agents working in this repository. People should read
[CONTRIBUTING.md](CONTRIBUTING.md); this file says the same things in the order an agent
needs them, adds the traps that cost time, and is kept short on purpose. Where the two
disagree, CONTRIBUTING.md wins: report the difference.

## What this is

giong is a thread-per-core async runtime for Linux, built on io_uring, with an epoll
driver where io_uring is blocked or the kernel is too old. One Cargo
workspace, nine crates and an example service:

| Path | Crate | What it is |
|---|---|---|
| `.` | `giong` | the runtime: executor, io_uring and epoll drivers, TCP/UDP/Unix sockets, files, timers, channels |
| `giong-http/` | `giong-http` | a strict HTTP/1.1 and HTTP/2 server library (bodies whole or streamed both ways; HTTP/2 through ALPN or prior knowledge, `src/h2/`), HTTPS behind the `tls` feature; an HTTP/1.1 and HTTP/2 client (proxies, gzip, streamed bodies; HTTP/2 in `src/client/h2.rs`) behind `client` |
| `giong-tls/` | `giong-tls` | TLS on rustls |
| `giong-postgres/` | `giong-postgres` | a PostgreSQL client and per-worker pool |
| `giong-mysql/` | `giong-mysql` | a MySQL client and per-worker pool |
| `giong-redis/` | `giong-redis` | a Redis client (RESP3) with Pub/Sub and a per-worker pool |
| `giong-web/` | `giong-web` | a router, middleware (request ids, JSON-line logs, CORS, gzip, timeouts; `tracing` spans behind `tracing`) and JSON on giong-http |
| `giong-mcp/` | `giong-mcp` | an MCP server that lets agents read a running service's workers, tasks, pools and profiles |
| `giong-compat/` | `giong-compat` | `Compat`: Tokio and futures-io `AsyncRead`/`AsyncWrite`/`AsyncBufRead` for giong's streams, for transport-generic crates (h2, tokio-rustls, tokio-tungstenite, hyper's `conn`) |
| `examples/rest-api/` | `rest-api` (not published) | a complete REST API: PostgreSQL, a Redis cache, HTTPS; its test needs PostgreSQL and redis-server |

`bench/` holds separate workspaces (the servers giong is compared with). Do not build them
unless the task is about benchmarks.

Where the code is:

| File | Holds |
|---|---|
| `src/runtime.rs` | `Builder`, workers, `block_on`, `run`, shutdown, `stats()`, load hints and `spawn_balanced` |
| `src/executor.rs` | tasks, `spawn`, `JoinHandle`, `RemoteJoinHandle` (for `Handle::spawn_on`) |
| `src/driver/mod.rs` | the driver facade: `Io` (one operation, driver-neutral), `DriverKind`, choosing a driver and falling back |
| `src/driver/uring.rs` | io_uring setup, submission and completion, the ring tiers |
| `src/driver/epoll.rs` | the epoll driver: readiness, edge-triggered registration, the per-task budget |
| `src/driver/blocking.rs` | the thread pool the epoll driver runs file operations on |
| `src/op.rs` | `Op`: an in-flight operation that owns its buffers until the kernel is done |
| `src/net.rs`, `src/net/` | TCP, UDP, Unix sockets; `src/net/balance.rs` for `TcpListener::bind_balanced` |
| `src/fs.rs`, `src/time.rs`, `src/sync.rs`, `src/buf.rs` | files, timers, channels, buffer pool |
| `src/notify.rs` | the cross-thread wake-up handshakes, model-checked with loom |
| `src/observe.rs` | `Handle::pulse`, `snapshot`, profiles and `Source`s: what giong-mcp reads |
| `tests/` | integration tests, one file per area; `tests/cancel.rs` for dropped operations |

## The machine you need

- **Linux 6.0 or newer with io_uring allowed**, to test both drivers. giong does not
  build on macOS or Windows.
- **Docker blocks io_uring with its default seccomp profile.** Inside such a container,
  or on an older kernel, the tests still pass, but on the epoll driver only: the io_uring
  half of the suite goes untested. `tests/driver.rs` and `tests/blocked.rs` check which
  driver a worker got. Use a VM, WSL2 with a 6.x kernel, or a container with the profile
  from `docs/DEPLOYMENT.md` to test io_uring.
- Stable Rust. The libraries must build on **Rust 1.80**; the tests need a newer compiler
  (rcgen's dependencies). Nightly with the `rust-src` component is needed for
  ThreadSanitizer only.
- **Database servers, for the database crates' tests.** giong-postgres, giong-mysql and
  giong-redis start their own throwaway server from the installed binaries (PostgreSQL's
  `initdb`/`postgres`, `mysqld`, `redis-server`) and skip, with a message, when they are
  missing. `GIONG_PG_REQUIRED=1`, `GIONG_MYSQL_REQUIRED=1` and `GIONG_REDIS_REQUIRED=1`
  turn a missing server into a failure, as CI does; each crate's README says where the
  binaries are looked for.

## Commands

Run these from the repository root. All tests use loopback; none needs the internet.

```bash
cargo build --workspace --all-features
cargo test --workspace --all-features            # everything, debug build
cargo test -p giong-http --all-features          # its HTTPS and client tests need the features

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo +1.80.0 check --workspace --all-features   # the minimum Rust version, libraries only

scripts/check.sh test        # release build, every test 5 times; ROUNDS=1 for one pass
scripts/check.sh fallback    # the same without optional features, on the ring setups of kernels
                             # older than 6.1, on epoll, then with submission polling (SQPOLL)
scripts/check.sh epoll       # the tests on the epoll driver only
scripts/check.sh sqpoll      # the tests with SQPOLL (GIONG_SQPOLL) only
scripts/check.sh seccomp     # io_uring blocked as in Docker (scripts/no-io-uring.py): giong picks epoll
scripts/check.sh tsan        # ThreadSanitizer (nightly + rust-src)
scripts/check.sh loom        # loom models of the cross-thread wake-ups (src/notify.rs)
scripts/check.sh soak        # minutes of connection churn, then checks for leaks
scripts/kernels.sh           # the suite on real 5.15 (epoll), 6.0 and 6.1 kernels in QEMU (slow)
KTLS=1 scripts/kernels.sh 5.15.225 6.1.155 6.18.63   # with the kernel's tls module loaded (kernel TLS)
scripts/valgrind.sh          # memcheck on the single-threaded suites
python3 scripts/release.py check   # READMEs, CHANGELOG and site agree with the version
```

Continuous integration requires five checks on every pull request: `lint` (fmt, clippy,
doc), `msrv`, `test`, `fallback` and `tsan`. A change is done when all five would pass.
CI also runs `loom`, the `kernels` jobs, the tests inside Docker's default seccomp
profile (`docker`) and the database crates against other server
versions (`servers`) on every pull request, and fuzzes the parsers (`giong-http`,
`giong-postgres`, `giong-mysql`, `giong-redis`, `giong-web`, each in its `fuzz/`) and runs
the soak test every night.

`GIONG_RING_TIER=coop` or `GIONG_RING_TIER=basic` forces an older ring setup in any test
run, `GIONG_IO_URING_OFF=all` (or a list of names such as `packed-buffers`) turns off the
optional io_uring features that newer kernels have (`giong::IoUringFeature`), and
`GIONG_DRIVER=epoll` picks the epoll driver; `scripts/check.sh fallback` does all three for
you. `GIONG_ZERO_COPY_MIN=<bytes>` moves the size from which TCP sends from bytes that cannot
change (`&'static [u8]`, `Rc<[u8]>`, `Arc<[u8]>`) go out without a copy (16 KiB), for
benchmarks.

The kernel TLS tests in `giong-tls` skip themselves when the kernel has no `tls` module
(`sudo modprobe tls` loads it; containers need it loaded on the host).
`GIONG_KTLS_REQUIRED=1` turns those skips into failures; CI sets it wherever the module
is loaded.

## Rules that are checked in review

1. **Cancellation safety.** io_uring keeps using an operation's memory until the kernel
   reports completion, whatever happens to the Rust future, and so does a blocking thread
   of the epoll driver. New operations are an `Io` variant run through `Op`, never raw
   submissions, and work on both drivers. Every new `async fn` says in its documentation
   whether it is cancel safe. Every new operation has a test that drops it mid-flight and then checks
   `giong::stats()` for leaked operations and buffers (examples in `tests/cancel.rs`).
   Run it on both drivers (`GIONG_DRIVER=epoll`).
2. **Tests for the normal path and the interrupted one.** A bug fix comes with a test that
   fails without it. No tight timing in tests: use generous upper bounds.
3. **`unsafe`** only with a `// SAFETY:` comment saying why it is sound, a test that would
   catch misuse, and clean `scripts/check.sh tsan` and `scripts/valgrind.sh` runs.
4. **No new dependency** without a reason stated in the pull request. The runtime depends
   on `io-uring`, `libc` and `socket2` only.
5. **Public items are documented.** Futures are not `Send` by design: tasks stay on their
   worker, and `Rc`/`RefCell` are fine. Buffers are passed by value and handed back, as in
   `write_all(buf)` returning `(io::Result<()>, buf)`.
6. **Errors explain themselves.** An error giong raises carries a code:
   `giong::error::Error::new(Code::…, what went wrong, how to fix it)`, turned into an
   `io::Error`. A new code goes into `src/error.rs` and gets a page in `docs/errors/`
   (`tests/error_docs.rs` checks both); codes are never reused or renumbered. Errors passed
   on from the kernel stay as they are. A message that starts with `GIONG-E0102` is
   explained in `docs/errors/E0102.md`. No unbounded defaults: every timeout and size
   limit is finite and documented.
7. **Wake-ups and the parsers have their own checks.** A change to `src/notify.rs`,
   the injector or `TaskWaker` updates its loom model in the same pull request. A change
   to a parser or connection state machine is fuzzed for a few minutes first (each crate's
   `fuzz/`, commands in CONTRIBUTING.md).
8. **Performance claims need numbers**: before and after, the median of several runs with
   the spread, the machine and kernel. Never write a number you did not measure.

## Changes, commits and pull requests

- Branch from `main` as `<type>/<short-description>`. Never push to `main`; it is
  protected.
- Commits follow Conventional Commits: `<type>(<scope>): <imperative summary, lower case,
  no full stop>`. Types: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `bench`,
  `ci`, `chore`. Scopes: `runtime`, `executor`, `driver`, `net`, `fs`, `time`, `sync`,
  `buf`, `http`, `tls`, `examples`, `bench`. A breaking change takes `!` after the scope
  and a `BREAKING CHANGE:` footer. The body says why.
- Pull requests are squash-merged, so the title must be a valid commit summary. Fill in
  `.github/PULL_REQUEST_TEMPLATE.md`, including how the change was tested.
- Anything a user would notice gets a line under `## [Unreleased]` in `CHANGELOG.md`
  ([Keep a Changelog](https://keepachangelog.com/en/1.1.0/)), grouped by crate.
- Documentation is in English only: the READMEs, the docs and the comments. Each crate has
  one `README.md`. `scripts/release.py check` verifies the images and install snippets in
  them, not the test counts or benchmark numbers: update those by hand when a change moves
  them.
- Releases are automated (`docs/RELEASING.md`). Do not bump versions or tag by hand.

## Traps

- **Several worktrees, one target directory.** ThreadSanitizer builds share
  `$CARGO_TARGET_DIR/tsan`; two worktrees using the same one give stale, misleading
  results. Give each worktree its own `CARGO_TARGET_DIR`.
- **valgrind and threads.** valgrind treats `io_uring_enter` as non-blocking, so a test
  with a second thread waiting in it stalls. `scripts/valgrind.sh` lists the suites and
  tests it leaves out for this reason; a timeout there is not a bug in giong.
- **`SIGTERM` under ThreadSanitizer.** `sigterm_triggers_a_graceful_shutdown` is skipped
  under TSan on purpose (see the comment in `scripts/check.sh`).
- **A fresh target directory** takes a few minutes for `scripts/check.sh tsan`, because it
  rebuilds the standard library.
- **Crates from the Tokio ecosystem.** Prefer the giong crate for the job (the table
  above). A crate that takes a stream and is driven by your task (h2, tokio-rustls,
  tokio-tungstenite, hyper's `conn`, tokio-util's codecs) runs over
  `giong_compat::Compat`; flush after writing, as with `BufWriter`. One that needs Tokio's
  reactor, timers or `tokio::spawn` panics with "there is no reactor running": `Compat`
  cannot fix that (see `giong-compat/README.md`).
- **Benchmarks are not tests.** Do not run `bench/*.sh` to check a change unless the task
  is about performance: they take long and need a quiet machine.

## Where the project is going

[ROADMAP.md](ROADMAP.md) has the milestones and the non-goals. Read the non-goals before
proposing anything large, and open an issue first for anything bigger than a fix.
