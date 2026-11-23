### Added

- **giong-cli** (new crate, binary `giong`): one command to start, run, check and ship
  services on giong. Every command prints text, or one JSON document with `--json`, and
  every problem it reports carries a `GIONG-E50xx` code with the cause and the fix.
  `giong explain <code>` prints the page of any giong error code offline.
- **giong**: `TcpListener::bind` takes the listening socket that `giong dev` hands down in
  `GIONG_DEV_LISTEN` for the same address, so a service restarted after a rebuild keeps its
  port and no connection is refused between two builds. Without the variable nothing
  changes. The hand-off is compiled only into debug builds and builds with the new `dev`
  feature (`giong dev --release` turns it on for its own builds): a release build has none
  of its code, and refuses to start under `giong dev` with a coded error.
- **giong-cli**: `giong dev` builds and runs the service and, on every save, builds again
  and swaps the new build in once it accepts connections: measured 0 failed requests over
  30 restarts under constant load (io_uring and epoll). A failed build, or a build that
  stops at start-up, leaves the running one serving. A change to `.env` alone restarts
  without building. It starts the service with `WORKERS=2` on a laptop, `.env`,
  `RUST_BACKTRACE=1` and a giong-mcp socket, and keeps `.giong/dev.json` and
  `.giong/dev.log` for other tools; `--json` prints one event per line. A `LISTEN` of
  `0.0.0.0` or `[::]` is served on loopback with a warning (`GIONG-E5024`) unless `--lan`
  is given, so a service under development is not on the network by accident.
- **giong-cli**: local development as one runner per project. `giong dev --background`
  starts `giong dev` detached from the terminal, waits until the service serves on
  localhost and prints its address (a failed build comes back as findings, and it builds
  again on the next save); `giong dev status` says what runs and how the last build went,
  `--all` across projects; `giong dev logs` prints the service's last lines; `giong dev stop`
  stops it gracefully, and the database container `giong db up` started, data kept. When a
  port is taken, the error names the process that holds it.
- **giong-cli**: `giong check` reports the compiler's errors and warnings, clippy's lints and
  giong's own checks of the source in one report: blocking calls in async code
  (`GIONG-E5006`), listeners bound to a host name (`GIONG-E5007`), `RefCell` borrows held
  across an `.await` (`GIONG-E5008`). Machine-applicable fixes come as edits.
- **giong-cli**: `giong mcp` serves giong's commands as tools to coding agents over MCP on
  standard input and output, each answering with the command's JSON document (`dev status`
  and `dev logs` read; `dev --background` and `dev stop` need `--allow-writes`). The tools only read unless `--allow-writes` is
  given; `db reset`, `db down`, `clean --all`, `env get`, `--reveal` and `image --push` are
  never tools, and every tool carries MCP's read-only, destructive and idempotent hints.
- **giong-cli**: every release attaches the `giong` command prebuilt for Linux (static,
  x86_64 and aarch64), macOS (Apple silicon and Intel) and Windows (x86_64), each with its
  SHA-256. `curl -fsSL https://giong.run/install.sh | sh` (or
  `powershell -c "irm https://giong.run/install.ps1 | iex"` on Windows) installs it in
  `~/.giong/bin` without root, checks the checksum, and builds it with `cargo install`
  where no binary is published.
