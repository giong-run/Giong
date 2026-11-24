# AGENTS.md

Instructions for AI coding agents working in this repository. People should read
[CONTRIBUTING.md](CONTRIBUTING.md); this file says the same things in the order an agent
needs them, adds the traps that cost time, and is kept short on purpose. Where the two
disagree, CONTRIBUTING.md wins: report the difference. How a change goes from an item to a
merge, with several agents working at once (preflight, changelog entries, conflicts between
pull requests, handoff), is in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).

## What this is

giong is a thread-per-core async runtime, built on io_uring on Linux, with an epoll
driver where io_uring is blocked or the kernel is too old, a kqueue driver on macOS (for
development) and an I/O completion port driver on Windows (the `giong` crate only, from
1.4). Linux is the platform it is tuned and measured on. It aims to be the best Rust
async runtime on technical merit, replaces the Tokio ecosystem with crates of its own, and
is built for code that agents write: strict types, coded errors with the cause and the
fix, and an API that stays on major version 1 for many years. One Cargo
workspace, thirteen crates and three examples:

| Path | Crate | What it is |
|---|---|---|
| `.` | `giong` | the runtime: executor, io_uring and epoll drivers, TCP/UDP/Unix sockets, files, timers, channels, child processes, pipes, signals |
| `giong-http/` | `giong-http` | a strict HTTP/1.1 and HTTP/2 server library (bodies whole or streamed both ways; HTTP/2 through ALPN or prior knowledge, `src/h2/`), HTTPS behind the `tls` feature; an HTTP/1.1 and HTTP/2 client (proxies, gzip, streamed bodies; HTTP/2 in `src/client/h2.rs`) behind `client`; HTTP/3 for both behind `unstable-http3` (Rust 1.85) |
| `giong-tls/` | `giong-tls` | TLS on rustls |
| `giong-postgres/` | `giong-postgres` | a PostgreSQL client and per-worker pool |
| `giong-mysql/` | `giong-mysql` | a MySQL and MariaDB client and per-worker pool |
| `giong-redis/` | `giong-redis` | a Redis client (RESP3) with Pub/Sub and a per-worker pool |
| `giong-web/` | `giong-web` | a router, middleware (request ids, JSON-line logs, CORS, gzip, timeouts; `tracing` spans behind `tracing`) and JSON on giong-http |
| `giong-mcp/` | `giong-mcp` | an MCP server that lets agents read a running service's workers, tasks, pools and profiles |
| `giong-compat/` | `giong-compat` | `Compat`: Tokio and futures-io `AsyncRead`/`AsyncWrite`/`AsyncBufRead` for giong's streams, for transport-generic crates (h2, tokio-rustls, tokio-tungstenite, hyper's `conn`) |
| `examples/rest-api/` | `rest-api` (not published) | a complete REST API: PostgreSQL, a Redis cache, HTTPS; its test needs PostgreSQL and redis-server; `Dockerfile` and `compose.yaml` run it in containers |
| `giong-proto/` | `giong-proto` (new in 1.1) | Protocol Buffers decoded as views over the receive buffer (no copy, no allocation); message structs for encoding; codes `GIONG-E4001`-`E4008`. `conformance/` holds Google's conformance testee and the differential tests against prost |
| `giong-grpc-build/` | `giong-grpc-build` (new in 1.1) | generates giong-proto messages and giong-grpc service traits, servers and clients from `.proto` files at build time, parsed in Rust by protox (no `protoc`) |
| `giong-grpc/` | `giong-grpc` (new in 1.1) | gRPC servers and clients on giong-http's HTTP/2: the four kinds of call, deadlines, cancellation, metadata, gzip, health and reflection; codes `GIONG-E4101`-`E4110`. `interop/run.sh` runs gRPC's interoperability cases against grpc-go (needs Go); `tests/generated/` and `src/generated/` hold generated code that `tests/generated.rs` keeps current (`GIONG_GRPC_REGENERATE=1` rewrites it) |
| `giong-quic/` | `giong-quic` (new in 1.3) | QUIC on giong's UDP sockets, one endpoint per worker, TLS 1.3 from rustls, the engine underneath quinn-proto; codes `GIONG-E3601`-`E3605`. Needs Rust 1.85, the one crate above giong's 1.80 |
| `examples/proto-hello/` | `proto-hello` (not published) | giong-grpc-build and giong-proto in one small program |
| `examples/grpc-hello/` | `grpc-hello` (not published) | a gRPC server and client: a unary method and a server stream, health and reflection |

`bench/` holds separate workspaces (the servers giong is compared with). Do not build them
unless the task is about benchmarks.

Where the code is:

| File | Holds |
|---|---|
| `src/runtime.rs` | `Builder`, workers, `block_on`, `run`, shutdown, `observe::worker_stats()`, load hints and `spawn_balanced` |
| `src/executor.rs` | tasks, `spawn`, `JoinHandle`, `RemoteJoinHandle` (for `Handle::spawn_on`) |
| `src/driver/mod.rs` | the driver facade: `Io` (one operation, driver-neutral), `DriverKind`, choosing a driver and falling back |
| `src/driver/uring.rs` | io_uring setup, submission and completion, the ring tiers |
| `src/driver/epoll.rs` | the epoll driver: readiness, edge-triggered registration, the per-task budget |
| `src/driver/blocking.rs` | the thread pool the epoll driver runs file operations on |
| `src/op.rs` | `Op`: an in-flight operation that owns its buffers until the kernel is done |
| `src/net.rs`, `src/net/` | TCP, UDP, Unix sockets; `src/net/addr.rs` for addresses and `ToSocketAddrs`, `src/net/dns.rs` for `lookup_host` and `Resolver`, `src/net/share.rs` for the ports `bind` shares between workers only, `src/net/balance.rs` for `TcpListener::bind_balanced` |
| `src/fs.rs`, `src/time.rs`, `src/sync.rs`, `src/buf.rs` | files, timers, channels, buffer pool |
| `src/process.rs`, `src/process/` | `Command` and `Child` (waited for through a pidfd), pipes and the process's own stdio (`pipe.rs`), the Linux calls (`sys.rs`) |
| `src/signal.rs` | `signal::listen`: the process-wide handlers and the `giong-signal` thread that wakes listeners |
| `src/net/fds.rs`, `src/net/activation.rs` | passing descriptors over Unix sockets (`SCM_RIGHTS`), Unix addresses and peer credentials; socket activation (`LISTEN_FDS`) |
| `src/error.rs` | `Code`: every `GIONG-Exxxx` code, of every crate |
| `src/notify.rs` | the cross-thread wake-up handshakes, model-checked with loom |
| `src/observe.rs` | `Handle::pulse`, `snapshot`, profiles and `Source`s: what giong-mcp reads |
| `src/observe/requests.rs` | request tracing: `RequestTrace`, `wait`, `set_route`, `take_requests` |
| `tests/` | integration tests, one file per area; `tests/cancel.rs` for dropped operations |

A service built on giong for a container (Docker, Kubernetes, a sandbox): start from
`examples/rest-api/Dockerfile`; set `Builder::shutdown_on_signals(true)`, and leave the
worker count to giong unless the pod has a CPU request and no limit. giong raises the soft
`RLIMIT_NOFILE` to the hard one itself (`Builder::raise_open_files_limit`). [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) has the why and
the numbers.

Which crate to use, in this repository and in a service built on giong: the giong crate
for the job first (HTTP server and client, routing, TLS, PostgreSQL, MySQL, Redis, files,
names, timers); then a crate that needs no runtime; then a Tokio-ecosystem crate that takes
a stream, over `giong_compat::Compat`, only for a protocol giong has no crate for; never a
crate that needs the Tokio runtime on a worker. The table with each giong crate, what it
replaces and how it measured against it is under
[Choosing crates](README.md#choosing-crates) in the README.

## Looking inside a running service

To find out why a giong service is slow, stuck or leaking, ask it rather than guess:
`giong-mcp` serves the runtime's own state over MCP. The service keeps
`let _mcp = giong_mcp::start_from_env(&handle);` alive; run it with
`GIONG_MCP=127.0.0.1:7777` and `GIONG_MCP_TOKEN` (16 characters or more), and connect with
`claude mcp add --transport http giong http://127.0.0.1:7777/mcp --header "Authorization: Bearer $GIONG_MCP_TOKEN"`.
Tools: `overview` (memory, threads, open descriptors, container limits and why this many
workers, warnings such as a blocked worker), `workers`, `tasks` (live tasks by the line that
spawned them), `sources` (database pools), `profile` (where the workers spent a time window)
and `slow_requests` (per route, where the time of the slowest requests went: pool waits,
queries, other services, the handler's code, a worker blocked by another task, with a verdict
and its confidence). It listens on loopback or a Unix socket only and only reads. Details: [giong-mcp/README.md](giong-mcp/README.md).

## The machine you need

- **Linux 6.0 or newer with io_uring allowed**, to test both Linux drivers. Every crate
  also builds and passes its tests on macOS (kqueue); on Windows only the `giong` crate
  builds (`cargo test -p giong`), and Unix sockets are `cfg(unix)`. A change to a shared
  file in `src/driver/` or `src/sys/` must keep all three building: CI runs macOS and
  Windows jobs, and `docs/design/portability.md` has the `cfg` rules.
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
  `initdb`/`postgres`, `mysqld` or `mariadbd`, `redis-server`) and skip, with a message, when they are
  missing. `GIONG_PG_REQUIRED=1`, `GIONG_MYSQL_REQUIRED=1` and `GIONG_REDIS_REQUIRED=1`
  turn a missing server into a failure, as CI does; each crate's README says where the
  binaries are looked for.

## Commands

Run these from the repository root. All tests use loopback; none needs the internet.

```bash
scripts/preflight.sh          # the gate before every push: every check CI runs that the changed files call for
scripts/preflight.sh --quick  # the fast part, while iterating
scripts/preflight.sh --against 172,173   # also merge with those open pull requests
scripts/preflight.sh --install           # the toolchains and tools the gate needs (CI's pinned stable, 1.80, 1.85, nightly, cargo-fuzz, semver-checks)

cargo build --workspace --all-features
cargo test --workspace --all-features            # everything, debug build
cargo test -p giong-http --all-features          # its HTTPS and client tests need the features
cargo check -p giong --target x86_64-pc-windows-msvc   # Windows, runtime crate only (rustup target add it first)

cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
scripts/msrv.sh                     # the minimum Rust versions (1.80, 1.85 for giong-quic), libraries only; CI runs the same

scripts/check.sh test        # release build, every test 5 times; ROUNDS=1 for one pass
scripts/check.sh fallback    # the same on the ring setups of kernels older than 6.1, then on epoll
scripts/check.sh epoll       # the tests on the epoll driver only
scripts/check.sh seccomp     # io_uring blocked as in Docker (scripts/no-io-uring.py): giong picks epoll
scripts/check.sh tsan        # ThreadSanitizer (nightly + rust-src)
scripts/check.sh loom        # loom models of the cross-thread wake-ups (src/notify.rs)
scripts/check.sh kani        # Kani proofs of the unsafe core (docs/kani.md; needs kani-verifier 0.68.0)
scripts/check.sh soak        # minutes of connection churn, plain and then over TLS, then checks for leaks
scripts/kernels.sh           # the suite on real 5.15 (epoll), 6.0 and 6.1 kernels in QEMU (slow)
KTLS=1 scripts/kernels.sh 5.15.225 6.1.155 6.18.63   # with the kernel's tls module loaded (kernel TLS)
scripts/valgrind.sh          # memcheck on the single-threaded suites
python3 scripts/release.py check   # READMEs, CHANGELOG and site agree with the version
giong-proto/conformance/run.sh     # Google's protobuf conformance suite (builds the runner once, ~10 min)
(cd giong-proto/conformance && cargo test --release)   # differential tests against prost
giong-grpc/interop/run.sh          # gRPC's interoperability cases against grpc-go, both directions (needs Go 1.24+)
scripts/semver.sh            # no breaking API change since the last release (cargo-semver-checks)
scripts/instr.py check       # instructions per operation and heap per idle connection vs bench/instr/baseline.csv
```

Continuous integration requires `all-checks` on every pull request, which passes when
every job of the workflow passed: `lint` (fmt, clippy, doc), `msrv`, `test`, `fallback`,
`tsan` and the jobs named below. A change is done when all of them would pass. The `plan`
job (`scripts/ci-scope.py`) picks how much a run does: a pull request ready for review runs
every job once (each suite one round, two ThreadSanitizer runs), a draft runs a short set
(lint, msrv, default features, the build, one round of the tests), and a pull request that
changes no file a build or test reads (the site, `changelog.d/`, prose) runs `lint` and
`msrv` only. A pull request is merged through the merge queue, which runs every job, with
every repetition (five rounds, three ThreadSanitizer runs), on `main` with the pull request
merged, and merges it only if they pass (docs/DEVELOPMENT.md, "CI tiers"). `main` then moves to the
commit that run tested, so a push to `main` (`.github/workflows/main.yml`) runs the jobs
again only when no merge queue run passed for that commit, or when the push changes one
of the two workflows.
The release test binaries are built once per run (the `build` job, with
`scripts/shared-build.sh`) for `test`, `docker`, the `kernels` jobs and the four
`fallback (…)` jobs, which run the tiers of `scripts/check.sh fallback` side by side, as
the `tsan (…)` jobs do for ThreadSanitizer's rounds; `fallback` and `tsan` pass when all
of theirs did.
CI also runs `semver` (`scripts/semver.sh`: `cargo-semver-checks` against the last
release, so no breaking change lands between releases; what the 1.x promise covers, and the
rules for growing the API without breaking it, are in `docs/STABILITY.md`), `instr` (`scripts/instr.py`:
instructions per operation on the hot paths, counted by callgrind, and heap per idle
connection, both within 1% of `bench/instr/baseline.csv`; a change meant to move them
commits the new baseline the job prints, see `bench/instr/README.md`), `kani` (the proofs
in `docs/kani.md`; a change to the slab, the buffer pool, `KernelBox` or the io_uring
operation lifecycle keeps them passing), `miri`, `loom`, the `kernels` jobs, the tests inside Docker's default seccomp
profile (`docker`), HTTP/2 conformance with h2spec (`h2spec`), HTTP/3 and QUIC with h3spec
(`h3spec`) and the database crates against other server versions (`servers`) on every pull
request, every crate's tests on macOS 15 and 26 (`macos.yml`) and the runtime's on Windows
Server 2022 and 2025 (`windows-runtime.yml`), and fuzzes the parsers
(`giong`, `giong-http`, `giong-tls`, `giong-postgres`, `giong-mysql`, `giong-redis`,
`giong-web`, `giong-mcp`, each in its `fuzz/`) and runs the soak test every night, on the
runner's kernel and on Linux 6.18 in QEMU; the 6.12 and 6.18 `kernels` jobs soak for a
minute on every pull request.

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
   `giong::observe::worker_stats()` for leaked operations and buffers (examples in `tests/cancel.rs`).
   Run it on both drivers (`GIONG_DRIVER=epoll`).
2. **Tests for the normal path and the interrupted one.** A bug fix comes with a test that
   fails without it. No tight timing in tests: use generous upper bounds.
3. **`unsafe`** only with a `// SAFETY:` comment saying why it is sound, a test that would
   catch misuse, and clean `scripts/check.sh tsan` and `scripts/valgrind.sh` runs.
4. **No new dependency** without a reason stated in the pull request. The runtime depends
   on `socket2`, plus `libc` on Unix, `io-uring` on Linux and `windows-sys` on Windows,
   and nothing else.
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
9. **The API rules.** Every public addition follows four rules: one way to do each thing;
   misuse fails at compile time, or at run time with a coded error; every behaviour code
   relies on is documented; a release only adds, and never changes what existing code
   means. Prefer a new item to a new meaning for an old one.

## Changes, commits and pull requests

- Branch from `main` as `<type>/<short-description>`. Never push to `main`; it is
  protected. Leave nothing behind: a superseded pull request is closed with a comment
  naming what replaces it and its branch deleted; a branch never stays without a pull
  request (docs/DEVELOPMENT.md, *Leave nothing behind*).
- Commits follow Conventional Commits: `<type>(<scope>): <imperative summary, lower case,
  no full stop>`. Types: `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `bench`,
  `ci`, `chore`. Scopes: the runtime's parts (`runtime`, `executor`, `driver`, `net`,
  `fs`, `ipc`, `time`, `sync`, `buf`), a companion crate by its name without `giong-` (`http`,
  `tls`, `web`, `postgres`, `mysql`, `redis`, `mcp`, `compat`), and `examples`, `bench`,
  `site`, `release`. Leave the scope out when a change spans several. A breaking change
  takes `!` after the scope and a `BREAKING CHANGE:` footer. The body says why.
- Pull requests are squash-merged by the merge queue, which writes the commit message
  itself: for a pull request of one commit it is that commit's message, so keep one commit
  whose message is the one `main` should get; for several commits it is the title and the
  list of their messages. Either way the title must be a valid commit summary. Fill in
  `.github/PULL_REQUEST_TEMPLATE.md`, including how the change was tested.
- Anything a user would notice gets an entry in a new file in `changelog.d/`
  ([changelog.d/README.md](changelog.d/README.md)), never an edit to `CHANGELOG.md`: pull
  requests in flight then do not conflict, and the release pull request merges the files
  into `CHANGELOG.md` ([Keep a Changelog](https://keepachangelog.com/en/1.1.0/) sections,
  entries grouped by crate).
- Documentation is in English only: the READMEs, the docs and the comments. Each crate has
  one `README.md`. `scripts/release.py check` verifies the images and install snippets in
  them, not the test counts or benchmark numbers: update those by hand when a change moves
  them.
- The public API was settled for 1.0 in 0.7.0 ([docs/reviews/2026-11-api-freeze.md](docs/reviews/2026-11-api-freeze.md)):
  add, do not break. A change that would break code written for 0.7 (a removed or renamed
  item, a changed signature, an enum made exhaustive) needs an issue first; additions to
  `#[non_exhaustive]` types and new methods are fine.
- A Rust sample in a README, `docs/DEPLOYMENT.md` or `docs/errors/README.md` is compiled by
  `cargo test -p rest-api --doc` (and run unless it is marked `no_run`): change the sample
  with the API. A new Markdown file with Rust samples goes into `markdown` in
  `examples/rest-api/src/lib.rs`.
- The projects `giong new` writes come from `giong-cli/templates/` (`common/` plus one
  directory per template; `{{var}}` and `{{#if flag}}` lines, see `giong-cli/src/template.rs`).
  `scripts/templates.sh` builds, lints and tests every combination against the checkout's
  crates (the `templates` workflow runs it, against real PostgreSQL, MySQL and Redis), so a
  change to an API a template uses fails there: change the template in the same pull
  request. `examples/rest-api` is the `web` template's output (except `tests/api.rs` and the
  doctests at the end of `src/lib.rs`): edit the template, then
  `GIONG_CLI_REGENERATE=1 cargo test -p giong-cli rest_api`.
- Releases are automated (`docs/RELEASING.md`). Do not bump versions or tag by hand.

## Traps

- **Pull requests in flight conflict in the merge queue.** One that merges first can put
  another out of date while the queue is testing it, and the queue ejects it. Before
  enqueueing, `scripts/preflight.sh --against <open pull requests>` names the files that
  conflict; the usual ones are README test counts and shared platform files. Two pull
  requests can also each pass alone and fail together: `bench/instr/baseline.csv` limits
  are the usual case (both add a little heap to the same path). Merge `main` in and run
  `scripts/instr.py check` again when the other one lands first.
- **A CI step named `net: ` is rerun by the CI gate when it fails** (docs/DEVELOPMENT.md,
  "When CI fails outside the code"). Only downloads and installs go there, through
  `scripts/ci-net.sh` or `.github/actions/apt-install`; a bare `apt-get` or `curl` in a
  workflow goes to a mirror every run and fails the whole run when the mirror does.
- **A push cancels the CI run in progress.** Push a finished step, not each edit: in
  November 2026 more than half of all pull request runs were cancelled by the next push
  after running a median of 7 minutes. Iterate locally (`scripts/preflight.sh --quick`, run
  by the pre-push hook) and as a draft.

- **TCP keeps no message boundaries.** On Linux 6.12 and newer, streams receive through
  packed buffer rings, and a receive ends wherever a shared buffer fills up: a test that
  takes one `recv()` for one message passes on older kernels and fails there. Read by
  length (`recv_exact` in the tests) or by a delimiter.

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
- **Crates from the Tokio ecosystem.** Prefer the giong crate for the job
  ([Choosing crates](README.md#choosing-crates)). A crate that takes a stream and is driven by your task (h2, tokio-rustls,
  tokio-tungstenite, hyper's `conn`, tokio-util's codecs) runs over
  `giong_compat::Compat`; flush after writing, as with `BufWriter`. One that needs Tokio's
  reactor, timers or `tokio::spawn` panics with "there is no reactor running": `Compat`
  cannot fix that (see `giong-compat/README.md`).
- **Signals are process-wide.** A test that installs a handler (`giong::signal::listen`)
  changes the whole test binary, so such tests live in `tests/signal.rs`. Tests that need
  a child process with a particular environment re-run their own test binary with an
  environment variable set (see `tests/process.rs` and `tests/activation.rs`).
- **gVisor is not Linux.** Its network stack records `SO_REUSEPORT` when a socket binds
  (Linux reads it when the next socket binds), its `somaxconn` is 1024, and it has no
  `SO_COOKIE` and no io_uring giong can use. Socket code that relies on when Linux reads
  an option needs a run of `scripts/gvisor.sh` (CI's `gvisor` job).
- **Benchmarks are not tests.** Do not run `bench/*.sh` to check a change unless the task
  is about performance: they take long and need a quiet machine.

## Where the project is going

[ROADMAP.md](ROADMAP.md) has the milestones and the non-goals. Read the non-goals before
proposing anything large, and open an issue first for anything bigger than a fix.
