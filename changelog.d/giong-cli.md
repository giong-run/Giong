### Added

- **giong-cli** (new crate, binary `giong`): one command to start, run, check and ship
  services on giong. Every command prints text, or one JSON document with `--json`, and
  every problem it reports carries a `GIONG-E50xx` code with the cause and the fix.
  `giong explain <code>` prints the page of any giong error code offline.
- **giong**: `TcpListener::bind` takes the listening socket that `giong dev` hands down in
  `GIONG_DEV_LISTEN` for the same address, so a service restarted after a rebuild keeps its
  port and no connection is refused between two builds. Without the variable nothing
  changes.
- **giong-cli**: `giong dev` builds and runs the service and, on every save, builds again
  and swaps the new build in once it accepts connections: measured 0 failed requests over
  30 restarts under constant load (io_uring and epoll). A failed build, or a build that
  stops at start-up, leaves the running one serving. A change to `.env` alone restarts
  without building. It starts the service with `WORKERS=2` on a laptop, `.env`,
  `RUST_BACKTRACE=1` and a giong-mcp socket, and keeps `.giong/dev.json` and
  `.giong/dev.log` for other tools; `--json` prints one event per line.
- **giong-cli**: `giong check` reports the compiler's errors and warnings, clippy's lints and
  giong's own checks of the source in one report: blocking calls in async code
  (`GIONG-E5006`), listeners bound to a host name (`GIONG-E5007`), `RefCell` borrows held
  across an `.await` (`GIONG-E5008`). Machine-applicable fixes come as edits.
- **giong-cli**: `giong mcp` serves every command as a tool to coding agents over MCP on
  standard input and output, each answering with the command's JSON document, plus
  `dev_status` for a running `giong dev`.
- **giong-cli**: every release attaches the `giong` command prebuilt for Linux (static,
  x86_64 and aarch64), macOS (Apple silicon and Intel) and Windows (x86_64), each with its
  SHA-256. `curl -fsSL https://giong.run/install.sh | sh` (or
  `powershell -c "irm https://giong.run/install.ps1 | iex"` on Windows) installs it in
  `~/.giong/bin` without root, checks the checksum, and builds it with `cargo install`
  where no binary is published.
