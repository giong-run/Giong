### Added

- **giong-cli**: `giong db up` starts a development PostgreSQL, MySQL or Redis in Docker or
  Podman (the engines the project depends on), with a random password, a free port and a
  labelled volume, and writes `DATABASE_URL` or `REDIS_URL` into `.env` (mode 0600,
  git-ignored). Each checkout gets its own container (package, engine and a short hash of the
  project's path), and a container `.giong/db.json` does not list is refused rather than given
  a guessed password (`GIONG-E5049`). `giong db new <name>`, `migrate` and `status` apply forward-only SQL files
  from `migrations/` (`NNNN_name.sql`), each PostgreSQL file in one transaction, record a
  SHA-256 checksum per file, refuse a changed applied file (`GIONG-E5043`) and name the line
  that failed (`GIONG-E5044`). `giong db reset --yes` and `down [--volumes]` complete it.
- **giong-cli**: `giong env` shows every key of `.env.example`, `.env` and the environment
  with where its value comes from, secrets masked; `giong env check` reports keys with no
  value at the line of `.env.example` that lists them, keys only in `.env`, and lines `.env`
  cannot parse; `giong env init` writes an owner-only, git-ignored `.env`; `giong env get
  <KEY>` prints one value for scripts.
- **giong-cli**: `giong top` shows a running service's workers, memory, descriptors, tasks
  by spawn line, pools and warnings through giong-mcp, refreshed live, or once with
  `--once`/`--json`; a worker stuck in one task is an error (`GIONG-E5055`).
- **giong-cli**: `giong bench` measures latency at a fixed rate without coordinated omission
  (each request counts from when it was due), at half the measured maximum by default, or
  throughput with `--rate max`. Without a URL it builds the service in release mode and
  starts it with every core. `giong bench --against <ref>` builds that commit in a git
  worktree and alternates the two builds round by round; `--max-regression <percent>` fails
  only when the change is slower in every round of at least three (`GIONG-E5064`). Every
  round goes to a CSV file.
- **giong-cli**: `giong image` builds an OCI image of the service without Docker or a
  Dockerfile: one layer with the release binary, CA certificates and a nonroot user, no
  shell, static with musl when that target is installed. The same source gives the same
  digest. `--load` hands it to Docker or Podman, `--push` to skopeo, crane, Docker or
  Podman.
