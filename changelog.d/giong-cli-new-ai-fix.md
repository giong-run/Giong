### Added

- **giong-cli**: `giong new <path> [--template web|grpc|worker] [--db postgres,mysql,redis]
  [--tls]` writes a project that builds and passes its tests from the first minute: one
  worker per core with graceful shutdown and giong-mcp, an end to end test, migrations,
  `.env.example`, a Dockerfile, a GitHub Actions workflow with the database services, and
  fast dev builds (`debug = "line-tables-only"`). `web` is a users API on PostgreSQL or MySQL
  (with Redis as a cache), counters on Redis, or a plain JSON service; `grpc` a service from a
  `.proto` (no `protoc`); `worker` jobs claimed from a table (`FOR UPDATE SKIP LOCKED`, with
  retries), from a Redis list, or on a timer. Every combination is built, linted and tested
  against real servers in CI (`scripts/templates.sh`), and `examples/rest-api` is now the
  `web` template's output.
- **giong-cli**: `giong ai init` (also run by `giong new`) writes `AGENTS.md`, `CLAUDE.md`, a
  Cursor rule, the Gióng skill of the release in `.claude/skills/giong/`, and the `giong` and
  `giong-service` MCP servers in `.mcp.json`. It can run again at any time: it replaces only
  its own marked block or files and never reorders or drops anything else.
- **giong-cli**: codes `GIONG-E5030` to `GIONG-E5039` and `GIONG-E5080` to `GIONG-E5082`,
  each with a page in `docs/errors/`.
