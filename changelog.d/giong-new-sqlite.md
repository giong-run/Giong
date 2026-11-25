### Added

- **giong-cli**: `giong new` writes the users API on SQLite: `--db sqlite`, and the web
  template's default, so a new project builds, passes its tests and serves with nothing to
  install or start (no Docker, no database server). The database is the file
  `data/<name>.db`, compiled in by rusqlite (feature `bundled`); the project's `src/db.rs`
  runs every query on giong's blocking lane (`giong::spawn_blocking`), with one write
  connection and a few read connections in WAL mode shared by every worker, so a query never
  holds up a worker and writes queue instead of failing with `database is locked`. Its tests
  use a file of their own each, its CI workflow starts no service, its Dockerfile keeps the
  file in a `/data` volume. `--db sqlite,redis` adds the Redis cache; `--db none` writes the
  service without a database (what `giong new` wrote without `--db` before).
- **giong-cli**: `giong db` works on SQLite projects (a rusqlite dependency): `up` starts
  nothing and writes `DATABASE_URL=sqlite:data/<name>.db` into `.env` when it names no SQLite
  file; `migrate`, `status` and `new` work through the `sqlite3` shell, each file in one
  transaction, also before `.env` exists; `reset --yes` drops every table of the file when
  it is inside the project and keeps the file, so a running service carries on.

### Changed

- **giong-cli**: `giong new` without `--db` writes the web template on SQLite instead of the
  hello service; `--db none` keeps the old output. The grpc and worker templates still
  default to no database.

### Fixed

- **giong-cli**: `giong db up` no longer adds `.giong/` to a `.gitignore` that lists
  `/.giong` (as every `giong new` project does).
