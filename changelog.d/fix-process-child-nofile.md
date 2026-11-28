### Fixed

- **giong**: a child started with `giong::process::Command` gets the soft open-files limit
  the process had before the runtime raised it (`Builder::raise_open_files_limit`), as Go
  does, so a child that uses `select(2)` no longer gets descriptors of 1,024 and more; a
  limit set by the caller's own `pre_exec` is kept
  ([#372](https://github.com/giong-run/Giong/issues/372)).
