### Added

- **giong-cli** (new crate, binary `giong`): one command to start, run, check and ship
  services on giong. Every command prints text, or one JSON document with `--json`, and
  every problem it reports carries a `GIONG-E50xx` code with the cause and the fix.
  `giong explain <code>` prints the page of any giong error code offline.
