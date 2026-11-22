### Added

- **giong-cli** (new crate, binary `giong`): one command to start, run, check and ship
  services on giong. Every command prints text, or one JSON document with `--json`, and
  every problem it reports carries a `GIONG-E50xx` code with the cause and the fix.
  `giong explain <code>` prints the page of any giong error code offline.
- **giong**: `TcpListener::bind` takes the listening socket that `giong dev` hands down in
  `GIONG_DEV_LISTEN` for the same address, so a service restarted after a rebuild keeps its
  port and no connection is refused between two builds. Without the variable nothing
  changes.
