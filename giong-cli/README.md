# giong-cli

The `giong` command: start, run, check and ship services on the [Gióng](https://github.com/giong-run/Giong) async runtime. One small binary (the standard library, `serde_json`, and `socket2` on Unix), the same on Linux, macOS and Windows.

```sh
curl -fsSL https://giong.run/install.sh | sh                 # Linux, macOS
powershell -c "irm https://giong.run/install.ps1 | iex"     # Windows
cargo install giong-cli --locked                            # anywhere Rust runs
```

The install scripts download the prebuilt binary of the latest release from GitHub, check its SHA-256 and put it in `~/.giong/bin` (`%USERPROFILE%\.giong\bin` on Windows) without asking for root; where no binary is published they build it with cargo. `GIONG_VERSION=1.5.0` picks a version, `GIONG_INSTALL=/path` another folder. Then:

```sh
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
giong dev [--bin NAME] [--release] [--listen ADDR[,ADDR]] [--workers N] [--prod-like] [--once] [-- service arguments]
```

Builds the service and runs it. On every save it builds again, starts the new build next to the old one, and swaps it in once it accepts connections: measured 0 failed requests over 30 restarts under constant load, on io_uring and on epoll. A build that fails, or one that stops at start-up, leaves the running build serving and prints the compiler's errors. A change to `.env` alone restarts without building.

How the swap works: `giong dev` binds the service's listening sockets itself and hands them down (`GIONG_DEV_LISTEN`). `giong::net::TcpListener::bind` on the same address takes the socket it was given instead of binding a new one, so the old and the new build accept on one socket and the kernel queues connections between them rather than refusing them. Once the new build accepts, the old one is told to stop accepting, finishes the connections it already took, and gets `SIGTERM` (`--grace` seconds, 30 by default, before `SIGKILL`). The runtime takes a socket only from the `giong dev` that started it; anywhere else `bind` binds as usual.

The listening addresses come from `--listen`, else `LISTEN` in the environment, else `LISTEN` in `.env`. Without one, with a giong older than 1.5, or on Windows, `giong dev` stops the old build before starting the new one: restarts still work, with a moment where the port is closed.

The service gets the variables of `.env` (those not already set), `WORKERS=2` so a laptop stays usable (`--workers N` sets another number, `--prod-like` leaves it to the service: every core), `RUST_BACKTRACE=1`, and `GIONG_MCP` set to a Unix socket so [giong-mcp](https://github.com/giong-run/Giong/tree/main/giong-mcp) can answer. Its output goes to the terminal and to `.giong/dev.log`; `.giong/dev.json` holds the state of the session (process ids, addresses, the last build and its errors) for other tools, and `--json` prints one event per line. `--once` builds, runs the service until it exits, and stops.

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

## giong db

```sh
giong db up            # a development PostgreSQL, MySQL or Redis in Docker or Podman
giong db new add_users # migrations/0002_add_users.sql
giong db migrate       # apply what is new; giong db status shows what is applied
giong db down          # stop it; --volumes also deletes the data
```

Starts the databases the project depends on in containers, with a random password, a free port and a volume, and writes `DATABASE_URL` or `REDIS_URL` into `.env` (owner-only, git-ignored). Migrations are forward-only SQL files `migrations/NNNN_name.sql`; each PostgreSQL file runs in one transaction (unless its first line is `-- giong:no-transaction`), each applied file is recorded with its SHA-256 checksum, a changed applied file is refused and a failing one names its line. With a `DATABASE_URL` of your own, `migrate` and `status` work on that database. `giong db reset --yes` empties a database `giong db up` started.

## giong env

```sh
giong env [show|check|init|get <KEY>]
```

The service's configuration comes from the environment only; `.env` is for development. `show` lists every key of `.env.example`, `.env` and the environment with where its value comes from, secrets masked (`--reveal` shows them). `check` reports a key with no value at the line of `.env.example` that lists it, a key only in `.env`, and a line `.env` cannot read. `init` writes `.env` from `.env.example`; `get` prints one value for scripts.

## giong top

```sh
giong top [--interval 1000] [--once] [--profile <ms>] [--tasks 20]
```

The running service seen from inside, through its giong-mcp: workers (tasks, I/O in flight, polls), memory, threads and descriptors, the tasks by the line that spawned them, pools, and warnings, such as a worker stuck in one task (`GIONG-E5055`). Redraws every second; one snapshot with `--once`, `--json` or when the output is not a terminal. Nothing is recorded while nobody watches, and nothing needs a rebuild.

## giong bench

```sh
giong bench [<url>] [--rate <n>|auto|max] [--conns 50] [--duration 10]
giong bench --against main [--rounds 5] [--max-regression 5]
```

Load at a fixed rate, with each request's latency counted from when it was due, so a stall is charged for every request behind it (no coordinated omission): p50, p99, p99.9 and the maximum. `auto` finds the most the service answers and measures at half of it; `max` measures throughput. Without a URL it builds the service in release mode and starts it on a free port with every core. `--against <ref>` builds that commit too and alternates the two builds round by round; `--max-regression` fails only when the change is slower in every round of at least three. Every round goes to `.giong/bench/<time>.csv`. The load generator shares the machine with the service: numbers to publish come from two machines.

## giong image

```sh
giong image [--tag name:tag] [--add static,migrations] [--load] [--push ghcr.io/owner/name:tag]
```

An OCI image of the service without Docker or a Dockerfile: one layer with the release binary at `/app`, CA certificates and a nonroot user, no shell; static with musl when that target is installed (`rustup target add x86_64-unknown-linux-musl`), else with this machine's glibc libraries. The same source gives the same digest. `--load` hands it to Docker or Podman, `--push` to skopeo, crane, Docker or Podman.

## Error codes

The command reports `GIONG-E5000` to `GIONG-E5099`; each has a page in [docs/errors](https://github.com/giong-run/Giong/blob/main/docs/errors/README.md) and in `giong explain`. Its findings can also carry the runtime's own codes, such as `GIONG-E0001` when io_uring is blocked.

## License

MIT or Apache-2.0, at your option.
