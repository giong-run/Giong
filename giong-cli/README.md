# giong-cli

The `giong` command: start, run, check and ship services on the [Gióng](https://github.com/giong-run/Giong) async runtime. One small binary (the standard library, `serde_json`, and `socket2` on Unix), the same on Linux, macOS and Windows.

```sh
curl -fsSL https://giong.run/install.sh | sh                 # Linux, macOS
powershell -c "irm https://giong.run/install.ps1 | iex"     # Windows
cargo install giong-cli --locked                            # anywhere Rust runs
```

The install scripts download the prebuilt binary of the latest release from GitHub, check its SHA-256 and put it in `~/.giong/bin` (`%USERPROFILE%\.giong\bin` on Windows) without asking for root; where no binary is published they build it with cargo. `GIONG_VERSION=1.5.0` picks a version, `GIONG_INSTALL=/path` another folder, `GIONG_DOWNLOAD_URL` a mirror of the release downloads. Then:

```sh
giong new api --db postgres   # a service that builds and passes its tests now
giong doctor     # is this machine (and project) ready?
giong dev        # build, run, and restart on every save without refusing a request
giong check      # compiler errors, clippy and giong's own checks in one report
```

## One shape for every command

Every command prints text for people, or one JSON document with `--json` for agents and scripts. Every problem it reports is a finding with a code, a severity, the place in the source when there is one, the cause and the fix:

```json
{"command": "check", "version": "1.5.0", "ok": true,
 "findings": [{"code": "GIONG-E5006", "severity": "warning",
               "title": "std::fs blocks the worker inside an async function", "detail": "...", "fix": "...",
               "url": "https://github.com/giong-run/Giong/blob/v1.5.0/docs/errors/E5006.md",
               "file": "src/main.rs", "line": 14, "column": 19, "fixable": false}],
 "data": {"tool": "cargo clippy", "took_ms": 812}}
```

The exit status is 0 when nothing is wrong, 1 when a finding is an error, and 2 when the command could not do its job at all (no project here, cargo missing). `giong explain GIONG-E5006` prints the page of any giong error code, of the runtime, every giong crate or this command, without the network. Fields of the JSON document are only ever added, never renamed.

## giong dev

```sh
giong dev [--bin NAME] [--release] [--listen ADDR[,ADDR]] [--lan] [--workers N] [--prod-like] [--once] [-- service arguments]
```

Builds the service and runs it. On every save it builds again, starts the new build next to the old one, and swaps it in once it accepts connections: measured 0 failed requests over 30 restarts under constant load, on io_uring and on epoll. A build that fails, or one that stops at start-up, leaves the running build serving and prints the compiler's errors. A change to `.env` alone restarts without building.

How the swap works: `giong dev` binds the service's listening sockets itself and hands them down (`GIONG_DEV_LISTEN`). `giong::net::TcpListener::bind` on the same address takes the socket it was given instead of binding a new one, so the old and the new build accept on one socket and the kernel queues connections between them rather than refusing them. Once the new build accepts, the old one is told to stop accepting, finishes the connections it already took, and gets `SIGTERM` (`--grace` seconds, 30 by default, before `SIGKILL`). The runtime takes a socket only from the `giong dev` that started it; anywhere else `bind` binds as usual. The hand-off is compiled into debug builds, and into release builds only with giong's `dev` feature, which `giong dev --release` and `--prod-like` turn on for their own builds; a release build without it, started by `giong dev`, refuses to bind with a coded error.

The listening addresses come from `--listen`, else `LISTEN` in the environment, else `LISTEN` in `.env`. `0.0.0.0` or `[::]` (every network interface, as a container in production wants) is served on loopback, `127.0.0.1` or `::1` with the same port, with a warning (`GIONG-E5024`); the service gets `LISTEN` set to the address `giong dev` listens on. `--lan` keeps the address, to reach the service from another device. Without one, with a giong older than 1.5, or on Windows, `giong dev` stops the old build before starting the new one: restarts still work, with a moment where the port is closed.

The service gets the variables of `.env` (those not already set), `WORKERS=2` so a laptop stays usable (`--workers N` sets another number, `--prod-like` leaves it to the service: every core), `RUST_BACKTRACE=1`, and `GIONG_MCP` set to a Unix socket so [giong-mcp](https://github.com/giong-run/Giong/tree/main/giong-mcp) can answer. Its output goes to the terminal and to `.giong/dev.log`; `.giong/dev.json` holds the state of the session (process ids, addresses, the last build and its errors) for other tools, and `--json` prints one event per line. `--once` builds, runs the service until it exits, and stops.

## giong new

```sh
giong new <dir> [--template web|grpc|worker] [--db postgres,mysql,redis] [--tls] [--name NAME] [--no-git] [--no-ai] [--dry-run]
```

Writes a project that builds, passes `clippy -D warnings` and passes its tests from the first minute, then runs `git init` and `giong ai init` in it. Every project has one worker per core with graceful shutdown on `SIGTERM`, giong-mcp (off until `GIONG_MCP` is set), an end to end test, `.env.example`, a Dockerfile, a GitHub Actions workflow with the database services it needs, and a dev profile that builds faster: on a PostgreSQL, Redis and TLS service a clean debug build takes 12.6 s instead of 15.3 s and the target directory 296 MB instead of 640 MB (Rust 1.100, [method](https://github.com/giong-run/Giong/tree/main/bench/results/devkit/template-profile)).

| Template | What it is |
|---|---|
| `web` (default) | A JSON API on giong-web with `/health` (and `/ready` when it has a database): a users API on PostgreSQL or MySQL (with Redis as a cache when both are chosen), counters on Redis, or a hello service without a database. Migrations in `migrations/`. |
| `grpc` | A gRPC service from `proto/service.proto` with health and reflection, built by giong-grpc-build (no `protoc` to install). |
| `worker` | A job runner: jobs claimed from a table with `FOR UPDATE SKIP LOCKED` (PostgreSQL) or in a transaction (MySQL), from a Redis list, or on a timer, with retries and backoff. |

`--tls` serves HTTPS or gRPC over TLS when `TLS_CERT` and `TLS_KEY` are set. `postgres` and `mysql` exclude each other, `grpc` takes no database and `worker` one, without `--tls`: anything else is refused with `GIONG-E5032` and the choices that work. The directory must be new or empty (`GIONG-E5031`); a write that fails takes back everything it made. Every combination is built, linted, tested on io_uring and on epoll against real databases, started and stopped with `SIGTERM` on every pull request.

## giong ai init

```sh
giong ai init [--dry-run]
```

Teaches the project to coding agents: `AGENTS.md` (the Gióng rules and this project's commands, read by Codex, Copilot, Cursor, Gemini and others), `CLAUDE.md` (imports `AGENTS.md`), `.cursor/rules/giong.mdc`, the Gióng skill of this release in `.claude/skills/giong/`, and the `giong` server in `.mcp.json`. Run it again at any time: in a file you wrote it replaces only its own marked block, a whole file only while it carries its `giong:owned` line, and in `.mcp.json` it adds its server when missing and keeps everything else. A file it cannot read is left alone, and a `CLAUDE.md` that links to `AGENTS.md` stays a link. It warns when the project uses another giong version than the command (`GIONG-E5038`).

## giong test

```sh
giong test [--quick] [--nextest] [--timeout SECS] [-- cargo test arguments]
```

Runs `cargo test` with the variables of `.env` (those not already set; cargo and rustc settings stay out, `GIONG-E5088`), then on Linux again with `GIONG_DRIVER=epoll`, the driver a container without io_uring gets. Each failing test is a `GIONG-E5083` finding naming the driver it failed on and what it printed. A run longer than `--timeout` (1800 s; 0 for none) is stopped with every process it started, naming the tests still running (`GIONG-E5086`). `--quick` runs once, `--nextest` uses cargo-nextest.

## giong check

```sh
giong check [--no-clippy] [-- cargo arguments]
```

Runs `cargo clippy` (or `cargo check`, when clippy is not installed or with `--no-clippy`) on every target of the workspace and adds giong's own checks of the source:

| Code | Finds |
|---|---|
| `GIONG-E5006` | a blocking call (`std::fs`, `std::thread::sleep`, `std::net::TcpStream::connect`, `std::process::Command`, `reqwest::blocking`, reading standard input) inside async code, where it stops every task of the worker |
| `GIONG-E5007` | a listener bound to a host name (`localhost:8080`), which binds whichever address the name resolves to first |
| `GIONG-E5008` | a `RefCell` borrow held across an `.await`, which panics when another task borrows it meanwhile |

Code in `spawn_blocking`, threads and tests is left out. Fixes that can be applied as they are come as `edits` in the JSON document. While `giong dev` runs, its last build is in the report too.

## giong fix

```sh
giong fix [--dry-run] [--allow-dirty] [-- cargo arguments]
```

Applies the fixes that need no judgement: the compiler's and clippy's machine-applicable suggestions and the `edits` of `giong check`'s findings, then looks again, up to three rounds. Each fix is applied whole or not at all, two fixes that touch the same code are never both applied, an edit outside the project is refused, and a file that changed while it ran is not written (`GIONG-E5082`). It refuses a git repository with changes that are not committed (`GIONG-E5080`) unless `--allow-dirty`; `--dry-run` shows the diff. What it leaves needs a decision: `giong check` lists it.

## giong doctor

```sh
giong doctor [--machine]
```

Checks the Rust toolchain and the linker, the kernel and whether io_uring is allowed, the locked-memory and open-files limits, and a container's seccomp profile; inside a project also the crates that need the Tokio runtime, a dev profile that writes full debug information (slow builds), `.env` against `.env.example`, the port the service uses, and the disk its builds take. Every problem comes with its code and its fix.

## giong clean

```sh
giong clean [dir...] [--days N] [--all] [--dry-run]
```

Frees the disk old builds use and keeps what the next build needs: removes what cargo built with another Rust version, and incremental data unused for 14 days, from this project's build directory, or from every cargo build directory under the folders given (`giong clean ~/code`). `--days N` also removes units unused for N days, `--all` empties the build directory, `--dry-run` only says what would go. It understands both of cargo's build-directory layouts and touches only directories cargo made.

## giong mcp

```sh
claude mcp add giong -- giong mcp
```

Serves every command as a tool to coding agents over the Model Context Protocol on standard input and output, each answering with the command's JSON document, plus `dev_status` for a running `giong dev`.

## Error codes

The command reports `GIONG-E5000` to `GIONG-E5099`; each has a page in [docs/errors](https://github.com/giong-run/Giong/blob/main/docs/errors/README.md) and in `giong explain`. Its findings can also carry the runtime's own codes, such as `GIONG-E0001` when io_uring is blocked.

## License

MIT or Apache-2.0, at your option.
