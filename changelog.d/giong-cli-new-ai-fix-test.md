### Added

- **giong-cli**: `giong new <path> [--template web|grpc|worker] [--db postgres,mysql,redis]
  [--tls]` writes a project that builds and passes its tests from the first minute: one
  worker per core with graceful shutdown and giong-mcp, an end to end test, migrations,
  `.env.example`, a Dockerfile, a GitHub Actions workflow with the database services, and
  fast dev builds (`debug = "line-tables-only"`: on a PostgreSQL, Redis and TLS service a clean
  debug build takes 12.6 s instead of 15.3 s, the target directory 296 MB instead of 640 MB,
  `bench/results/devkit/template-profile/`). `web` is a users API on PostgreSQL or MySQL
  (with Redis as a cache), counters on Redis, or a plain JSON service; `grpc` a service from a
  `.proto` (no `protoc`); `worker` jobs claimed from a table (`FOR UPDATE SKIP LOCKED`, with
  retries), from a Redis list, or on a timer. Every combination is built, linted and tested
  against real servers in CI (`scripts/templates.sh`), and `examples/rest-api` is now the
  `web` template's output. A service listens on `127.0.0.1` unless `LISTEN` says otherwise,
  so in development nobody else on the network reaches it; the Dockerfile sets `0.0.0.0`.
- **giong-cli**: `giong ai init` (also run by `giong new`) writes `AGENTS.md`, `CLAUDE.md`, a
  Cursor rule, the Gióng skill of the release in `.claude/skills/giong/`, and the `giong` MCP
  server in `.mcp.json`. They teach the agent one way to work on the service, like
  `npm run dev`: `giong dev --background` once, edit the code and let it rebuild, `curl` the
  address it prints, `giong dev status` and `giong dev logs` for errors and requests,
  `giong dev stop` at the end, and never `cargo run`, a release build or Docker for it. It
  can run again at any time: it replaces only its own marked block or files and never
  reorders or drops anything else. It never writes through a symbolic link a repository
  commits (`GIONG-E5036`), except `CLAUDE.md` and `AGENTS.md` naming each other.
- **giong-cli**: `giong fix` applies the fixes that need no judgement: the compiler's and
  clippy's machine-applicable suggestions and `giong check`'s own, in the project's files only,
  each whole or not at all, never two that overlap. It refuses a working tree with
  uncommitted changes (`--allow-dirty` to go ahead), and `--dry-run` shows the diff.
- **giong-cli**: `giong test` runs the suite with the variables of `.env`, then again with
  `GIONG_DRIVER=epoll` on Linux (the driver a container without io_uring gets), and reports
  each failure with its driver and its output. A run that takes over 30 minutes
  (`--timeout`) is stopped with every process it started, naming the tests still running;
  cargo and rustc settings in `.env` are left out of the build. `--quick` runs once,
  `--nextest` uses cargo-nextest.
- **giong-cli**: codes `GIONG-E5030` to `GIONG-E5039` and `GIONG-E5080` to `GIONG-E5088`,
  each with a page in `docs/errors/`.
