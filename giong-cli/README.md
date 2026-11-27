# giong-cli

The `giong` command: start, run, check and ship services on the [Gióng](https://github.com/giong-run/Giong) async runtime. One small binary (the standard library, `serde_json`, and `socket2` on Unix), the same on Linux, macOS and Windows.

```sh
curl -fsSL https://giong.run/install.sh | sh                 # Linux, macOS
powershell -c "irm https://giong.run/install.ps1 | iex"     # Windows
cargo install giong-cli --locked                            # anywhere Rust runs
```

The install scripts download the prebuilt binary of the latest release from GitHub, check its SHA-256 and put it in `~/.giong/bin` (`%USERPROFILE%\.giong\bin` on Windows) without asking for root; where no binary is published they build it with cargo. `GIONG_VERSION=1.5.0` picks a version, `GIONG_INSTALL=/path` another folder, `GIONG_DOWNLOAD_URL` a mirror of the release downloads.

Each release archive, and the binary in it, has a signed [build provenance attestation](https://docs.github.com/en/actions/security-guides/using-artifact-attestations-to-establish-provenance-for-builds) saying which workflow built it from which commit, and a CycloneDX SBOM (`giong-<version>-<target>.cdx.json` on the Release) listing every crate compiled in. With the GitHub CLI, check an installed command or a downloaded archive:

```sh
gh attestation verify ~/.giong/bin/giong --repo giong-run/Giong \
  --signer-workflow giong-run/Giong/.github/workflows/release.yml
```

The binary also carries that list of crates itself ([cargo auditable](https://github.com/rust-secure-code/cargo-auditable), a few hundred bytes), so `cargo audit bin ~/.giong/bin/giong`, Trivy, Grype or osv-scanner can check it for known advisories. Then:

```sh
giong new api    # a users API on SQLite that builds and passes its tests now, nothing to install
giong doctor     # is this machine (and project) ready?
giong dev --background  # build, run on localhost and rebuild on every save; returns once it serves
giong dev status        # what runs, where, and how the last build went (logs, stop: the same way)
giong check      # compiler errors, clippy and giong's own checks in one report
giong upgrade    # move the project (and this command) to the newest giong release
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
giong dev [--background] [--bin NAME] [--release] [--listen ADDR[,ADDR]] [--lan] [--workers N] [--prod-like] [--once] [--ui] [--ui-port N] [-- service arguments]
giong dev status [--all]
giong dev logs [--lines N]
giong dev stop [--all]
```

Builds the service and runs it. On every save it builds again, starts the new build next to the old one, and swaps it in once it accepts connections: measured 0 failed requests out of 102,061 over 100 restarts under constant load ([method and raw output](../bench/results/dev-restarts/)). A response in progress, a streamed one too, finishes on the old build when it ends within the grace, the smaller of the service's `shutdown_grace` (10 s in the web template) and `--grace` (30 s); a longer one is cut there, without the end of its body ([streamed responses over 50 restarts](../bench/results/dev-restarts-streaming/)). It never serves code older than the files: a change that does not compile, or that compiles and then stops before it serves (a panic at start-up, a port it cannot bind, no bind within `--ready-timeout`), stops the running build too and closes the port, so a request is refused at once rather than answered by the old code or left waiting. The terminal shows the compiler's errors or the service's last lines with a coded error and its fix, `giong dev status` says `failed` with the same (`failure` in its JSON), and the dashboard shows it at the top; the next save builds and starts again. A service that stops by itself later is reported the same way and waits for a save, so a crash at start-up does not loop. A change to `.env` alone restarts without building.

How the swap works: `giong dev` binds the service's listening sockets itself and hands them down (`GIONG_DEV_LISTEN`). `giong::net::TcpListener::bind` on the same address takes the socket it was given instead of binding a new one, so the old and the new build accept on one socket and the kernel queues connections between them rather than refusing them. Once the new build accepts, the old one is told to stop accepting, finishes the connections it already took, and gets `SIGTERM` (`--grace` seconds, 30 by default, before `SIGKILL`). The runtime takes a socket only from the `giong dev` that started it; anywhere else `bind` binds as usual. The hand-off is compiled into debug builds, and into release builds only with giong's `dev` feature, which `giong dev --release` and `--prod-like` turn on for their own builds; a release build without it, started by `giong dev`, refuses to bind with a coded error.

The listening addresses come from `--listen`, else `LISTEN` in the environment, else `LISTEN` in `.env`. `0.0.0.0` or `[::]` (every network interface, as a container in production wants) is served on loopback, `127.0.0.1` or `::1` with the same port, with a warning (`GIONG-E5024`); the service gets `LISTEN` set to the address `giong dev` listens on. `--lan` keeps the address, to reach the service from another device. Without one, with a giong older than 1.5, or on Windows, `giong dev` stops the old build before starting the new one: restarts still work, with a moment where the port is closed.

One project has one `giong dev`, and it is the way to run the service while developing: not `cargo run`, `cargo build --release`, a container or a second terminal. `--background` starts it detached from the terminal (its own process group: Ctrl-C or the end of an agent's shell does not stop it), waits until the first build serves, prints its address and returns; a build that fails comes back as findings, and the runner stays to build again on the next save. Started again while one runs, it reports that one. `giong dev status` says what runs (`state`: `building`, `starting`, `serving` or `failed`; address, process ids, the last build, why nothing serves when nothing does, the containers giong started for the project), `--all` for every project on this machine (`~/.giong/run`); `giong dev logs` prints the service's last lines; `giong dev stop` asks the runner to stop (it stops its service gracefully), signals it only if it does not, and stops what giong started for the project besides (`.giong/processes.json`: a database container from `giong db up`, its data kept). When the port is taken, when `giong dev` binds it or when the service does, the error names the process that holds it (Linux, macOS, Windows), and the project when it is another `giong dev`.

The service gets the variables of `.env` (those not already set), `WORKERS=2` so a laptop stays usable (`--workers N` sets another number, `--prod-like` leaves it to the service: every core), `RUST_BACKTRACE=1`, and `GIONG_MCP` set to a Unix socket so [giong-mcp](https://github.com/giong-run/Giong/tree/main/giong-mcp) can answer. Its output goes to the terminal and to `.giong/dev.log`; `.giong/dev.json` holds the state of the session (process ids, addresses, the last build and its errors) for other tools, and `--json` prints one event per line. `--once` builds, runs the service until it exits, and stops. `--ui` also serves the [dashboard](#giong-dashboard) on `http://127.0.0.1:7700` (or the first free port of the nine after it; `--ui-port N` picks another start); when all ten are taken the service runs without it and a `GIONG-E5053` warning says so.

## giong new

```sh
giong new <dir> [--template web|grpc|worker] [--db sqlite|postgres|mysql|none[,redis]] [--tls] [--name NAME] [--no-git] [--no-ai] [--dry-run]
```

Writes a project that builds, passes `clippy -D warnings` and passes its tests from the first minute, then runs `git init` and `giong ai init` in it. Every project has one worker per core with graceful shutdown on `SIGTERM`, giong-mcp (off until `GIONG_MCP` is set), an end to end test, `.env.example`, a Dockerfile, a GitHub Actions workflow with the database services it needs, and a dev profile that builds faster: on a PostgreSQL, Redis and TLS service a clean debug build takes 12.6 s instead of 15.3 s and the target directory 296 MB instead of 640 MB (Rust 1.100, [method](https://github.com/giong-run/Giong/tree/main/bench/results/devkit/template-profile)).

| Template | What it is |
|---|---|
| `web` (default) | A JSON API on giong-web with `/health` (and `/ready` when it has a database): a users API on SQLite by default, or on PostgreSQL or MySQL with `--db` (with Redis as a cache when `redis` is added), counters on Redis (`--db redis`), or a hello service without a database (`--db none`). Migrations in `migrations/`. |
| `grpc` | A gRPC service from `proto/service.proto` with health and reflection, built by giong-grpc-build (no `protoc` to install). |
| `worker` | A job runner: jobs claimed from a table with `FOR UPDATE SKIP LOCKED` (PostgreSQL) or in a transaction (MySQL), from a Redis list, or on a timer, with retries and backoff. |

SQLite needs nothing installed or started: the database is the file `data/<name>.db`, made at the first start, compiled in through rusqlite (feature `bundled`). Its calls block, so the project's `src/db.rs` runs each one on giong's blocking lane (`giong::spawn_blocking`), with one write connection and a few read connections in WAL mode shared by every worker; the tests use a file of their own each, and the CI workflow starts no service. One copy of the service owns a file; for several machines start from `--db postgres`.

`--tls` serves HTTPS or gRPC over TLS when `TLS_CERT` and `TLS_KEY` are set. `sqlite`, `postgres` and `mysql` exclude each other, `grpc` takes no database and `worker` one, without `--tls`: anything else is refused with `GIONG-E5032` and the choices that work. The directory must be new or empty (`GIONG-E5031`); a write that fails takes back everything it made. Every combination is built, linted, tested on io_uring and on epoll against real databases, started and stopped with `SIGTERM` on every pull request.

## giong ai init

```sh
giong ai init [--dry-run]
```

Teaches the project to coding agents: `AGENTS.md` (the Gióng rules and this project's commands, read by Codex, Copilot, Cursor, Gemini and others), `CLAUDE.md` (imports `AGENTS.md`), `.cursor/rules/giong.mdc`, the Gióng skill of this release in `.claude/skills/giong/`, and the `giong` server in `.mcp.json`. Run it again at any time: in a file you wrote it replaces only its own marked block, a whole file only while it carries its `giong:owned` line, and in `.mcp.json` it adds its server when missing and keeps everything else. A file it cannot read is left alone, and a `CLAUDE.md` that links to `AGENTS.md` stays a link; it writes through no other link (a repository can commit `.mcp.json` as a link to a file elsewhere) and says so (`GIONG-E5036`). It warns when the project uses another giong version than the command (`GIONG-E5038`).

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
giong doctor [--machine] [--offline]
```

Checks the Rust toolchain and the linker, the kernel and whether io_uring is allowed, the locked-memory and open-files limits, and a container's seccomp profile; inside a project also the crates that need the Tokio runtime, a dev profile that writes full debug information (slow builds), `.env` against `.env.example`, the port the service uses, and the disk its builds take. Every problem comes with its code and its fix.

In a project that takes giong crates from crates.io, it also reads `Cargo.lock`: giong crates on different releases (`GIONG-E5091`), a newer giong release out (`GIONG-E5090`) and this command older than it (`GIONG-E5092`). The newest release comes from `cargo info giong`, with your cargo configuration, at most once a day (`~/.giong/cache/releases.json`); `--offline` or `CARGO_NET_OFFLINE=true` skips the question, and `GIONG_LATEST=1.5.0` answers it (a mirror, or a company that approves releases).

## giong upgrade

```sh
giong upgrade [--to 1.5.0] [--dry-run] [--offline]
giong upgrade --self [--to 1.5.0] [--dry-run]
```

Moves every giong crate of the project to the newest release (or `--to`). The giong crates are released together with one version, and a 1.x release only adds, so this is one step: the version of each giong dependency in every `Cargo.toml` of the workspace, written as you wrote it (`"1.4"` becomes `"1.5"`, `"~1.4.2"` becomes `"~1.5.0"`; comments, features and every other byte stay), `cargo update` of the giong crates to exactly that release, the release's `rust-version` against rustc and the project's own `rust-version`, and `cargo check`. If a step fails, every file is put back as it was (`.giong/upgrade/` holds the originals while it runs). Then the agent files `giong ai init` wrote are refreshed for the new release. It never moves back, across a major version, or past a pinned requirement (`=1.4.2`: `GIONG-E5095`); path and git dependencies are left to you. `--dry-run` lists the edits; `--offline` moves to this command's own release with cargo offline.

This command writes templates and agent files for its own release, so a release newer than the command updates the command first, then the new command moves the project. `giong upgrade --self` updates only the command, the way it was installed: from the install script's folder with the release archive (checked against its SHA-256, `GIONG_DOWNLOAD_URL` honoured), or with `cargo install giong-cli --locked --version x.y.z`. A command installed another way is left alone, with the command to use (`GIONG-E5096`). In `giong mcp`, `upgrade --dry-run` reads and `upgrade` writes; neither ever replaces the command.

## giong clean

```sh
giong clean [dir...] [--days N] [--all] [--dry-run]
```

Frees the disk old builds use and keeps what the next build needs: removes what cargo built with another Rust version, and incremental data unused for 14 days, from this project's build directory, or from every cargo build directory under the folders given (`giong clean ~/code`). `--days N` also removes units unused for N days, `--all` empties the build directory, `--dry-run` only says what would go. It understands both of cargo's build-directory layouts and touches only directories cargo made.

## giong cache

```sh
giong cache [status]
giong cache share [dir...]
giong cache gc [--budget SIZE] [--dry-run]
```

Keeps one copy on disk of each dependency build for every project on the machine. Each Rust project builds its own copy of every dependency, and cargo never removes one; the build store replaces the files a registry or git dependency's build wrote with hard links to one copy in `~/.giong/store` whenever the bytes are the same. Each project keeps its own `target/`, so there is no shared lock to wait on, no binary of one project overwriting another's, and `cargo clean` in one project drops only that project's links. Cargo sees every build as fresh afterwards: the shared files get the same modification time in every project (2020-01-01 plus the unit's height in the dependency graph), so a unit stays newer than what it depends on. A build directory is changed only while giong holds cargo's own lock on it.

`giong dev` shares each build in the background once the service is ready, and runs `gc` once an hour. `giong cache share` does it for projects already built, giong or not (`giong cache share ~/code`). `giong cache gc` removes what no project links any more, and when the builds of the projects giong knows use more than the budget (`--budget`, `GIONG_STORE_BUDGET`, default `30G`, `off` for none) frees what an older Rust built and incremental data unused for 14 days, then the whole build of projects not built for 14 days, the longest idle first; never the project `giong dev` serves, never a build cargo is using (of any cargo version), and only directories cargo marked as builds (its `CACHEDIR.TAG`). `giong cache` says what the store saves. `GIONG_STORE=off` turns it off; `GIONG_STORE_DIR` moves it (it must be on the same drive as the projects). Needs cargo 1.100 or later (its build-dir layout) and a `giong` built with Rust 1.89 or later; anything else is left as it is (`GIONG-E5025`).

## giong mcp

```sh
claude mcp add giong -- giong mcp
```

Serves every command as a tool to coding agents over the Model Context Protocol on standard input and output, each answering with the command's JSON document. `upgrade` is a tool with `--dry-run` (read-only) and, with `--allow-writes`, without it; `--self` never is. `dev` is a tool for `status` and `logs` (read-only) and, with `--allow-writes`, `--background` and `stop`; `dev_status` reads `.giong/dev.json` as it is.

## giong routes and giong openapi

```sh
giong routes [--mcp ADDRESS] [--base-url URL]
giong openapi [--out FILE] [--title TEXT] [--mcp ADDRESS] [--base-url URL]
```

`giong routes` asks the running service which routes its giong-web apps serve: method, pattern, path parameters, streamed bodies, middleware, the line that added each route, and a curl line to try it. The service needs giong-mcp with its `web` feature; it is found through `.giong/dev.json` while `giong dev` runs, or `--mcp` (`GIONG_MCP` and `GIONG_MCP_TOKEN` otherwise).

`giong openapi` writes an OpenAPI 3.1 document from the same list: every path and method, path parameters, the coded problem+json answers giong-web makes itself (a parameter that does not parse, a body too long or not JSON, a handler that failed or timed out) and a curl sample per operation. Bodies are marked as not described, because giong-web reads them at run time and a guessed schema would mislead. Operation ids are unique, methods keep their case (only the standard upper-case ones are OpenAPI operations; others are listed under `x-giong-other-methods`), and routes of one shape share one path.

## giong record

```sh
giong record list [--route TEXT] [--method M] [--status 404|4xx] [--limit N] [--from LOG]
giong record show NUMBER
giong record save NUMBERS|3-7|last|all [--name NAME | --out FILE] [--append | --force] [--keep-secrets]
giong record replay [FILE...] [--base-url URL] [--timeout-ms N]
```

Turns real requests into tests. Add giong-web's `Record` middleware to the router; while `giong dev` runs, every answered request is written to `.giong/dev.log` with its headers and bodies, secret headers redacted. `list` and `show` read them, and `save` writes the chosen ones to `tests/recorded/<name>.json` as cases:

- dates, UUIDs, hex ids and tokens the service made become placeholders such as `{{uuid}}` and `{{time}}`
- an id the service made and a later request uses is captured (`{{capture:user_id}}`) and filled in again, so the cases pass against a service whose ids start elsewhere
- values the request itself sent stay literal, and the answer refers to them by name (`"no user {{user_id}}"`)
- secrets stay out of the file: redacted headers, and request fields whose names say secret (`password`, `token`, `api_key`, `client_secret`, `card_number`, `cvv` and the like) in the query, a form or a JSON body, are read from `GIONG_RECORD_<NAME>` when the cases run (`card.number` from `GIONG_RECORD_CARD_NUMBER`); such fields in answers become `{{string}}`
- `save` refuses, with `GIONG-E5050`, to write a value that still looks like a secret: a card number (by the Luhn check), a JWT, or a value under a secret name. `--keep-secrets` saves it anyway, for test values such as the `4242 4242 4242 4242` test card

`replay` sends the cases in order to the running service (or `--base-url`; from `.giong/dev.json` it takes only an address on this machine, so a cloned repository cannot send your environment's values elsewhere) and checks each answer: the status, `Content-Type`, the headers recorded and the body field by field. A difference is a `GIONG-E5066` error at the case's line with the expected and actual values, so `giong record replay` fails a CI run the way a test does. Case files are plain JSON, meant to be read, edited and committed.

## giong dashboard

```sh
giong dashboard [--port N] [--mcp ADDRESS] [--base-url URL] [--log FILE]
```

One page in the browser for the service `giong dev` runs in this project, on `http://127.0.0.1:7700`:

- the build state and the last compiler error
- the live request log, with status and time taken, and bodies when `Record` is on; each request can be copied as curl, sent again, or saved as a `giong record` case
- routes, with a Try button, curl lines and an OpenAPI download
- workers and tasks by the line that spawned them, with a one-second profile
- connection pools, the service's own output, and its warnings with their error pages
- a box to send any request to the service

It reads what the service already gives, its output and its giong-mcp, so the service runs no extra code for it. Only the person who started it can use it. It listens on 127.0.0.1 and answers only a loopback `Host`. Each start makes a random token, which is part of the address printed (`http://127.0.0.1:7700/#t=...`), and the API refuses any request without it (`GIONG-E5051`). The token comes after `#`, so no request line, log or `Referer` carries it, and the page takes it out of the address bar once read. A web page cannot read the dashboard or send requests through it, and neither can another program or user on the machine, though the request log holds bodies. `giong dev --ui` starts it with the service and prints its address.

## giong db

```sh
giong db up            # a development PostgreSQL, MySQL or Redis in Docker or Podman (SQLite: nothing to start)
giong db new add_users # migrations/0002_add_users.sql
giong db migrate       # apply what is new; giong db status shows what is applied
giong db down          # stop it; --volumes also deletes the data
```

Starts the databases the project depends on in containers, with a random password, a free port and a volume, and writes `DATABASE_URL` or `REDIS_URL` into `.env` (owner-only, git-ignored). Each checkout gets its own container, named after the package, the engine and a short hash of the project's path (`giong-shop-postgres-3fa2c1`), so two copies of a project never share a database. The container is recorded in `.giong/processes.json`: `giong dev status` lists it and `giong dev stop` stops it, keeping its data. Migrations are forward-only SQL files `migrations/NNNN_name.sql`; each PostgreSQL file runs in one transaction (unless its first line is `-- giong:no-transaction`), each applied file is recorded with its SHA-256 checksum, a changed applied file is refused and a failing one names its line. With a `DATABASE_URL` of your own, `migrate` and `status` work on that database. `giong db reset --yes` empties a database `giong db up` started.

SQLite (a project that depends on rusqlite) has no server: `giong db up` only writes `DATABASE_URL=sqlite:data/<name>.db` into `.env` when it names no SQLite file yet, and without it `migrate` and `status` use the file the service opens by default. They run through the `sqlite3` shell (apt install sqlite3, brew install sqlite; macOS has it), each file in one transaction. `giong db reset --yes` drops every table of the file when the file is inside the project, and keeps the file, so a running service carries on with the emptied one.

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

An OCI image of the service without Docker or a Dockerfile: one layer with the release binary at `/app`, CA certificates and a nonroot user, no shell, and `LISTEN=0.0.0.0:<port>` set as the Dockerfile from `giong new` sets it (for a SQLite project, the file in a `/data` volume, as there too); static with musl when that target is installed (`rustup target add x86_64-unknown-linux-musl`), else with this machine's glibc libraries. The same source gives the same digest. `--load` hands it to Docker or Podman, `--push` to skopeo, crane, Docker or Podman.

## Error codes

The command reports `GIONG-E5000` to `GIONG-E5099`; each has a page in [docs/errors](https://github.com/giong-run/Giong/blob/main/docs/errors/README.md) and in `giong explain`. Its findings can also carry the runtime's own codes, such as `GIONG-E0001` when io_uring is blocked.

## License

MIT or Apache-2.0, at your option.
