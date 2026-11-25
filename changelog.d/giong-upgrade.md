### Added

- **giong-cli**: `giong upgrade` moves every giong crate of a project to the newest release
  (or `--to`): the version of each giong dependency in every `Cargo.toml` of the workspace,
  written as it was written (`"1.4"` becomes `"1.5"`), then `cargo update`, the release's
  `rust-version` against rustc and the project's own, and `cargo check`; every file is put
  back if a step fails. It refreshes the agent files `giong ai init` wrote, updates the
  command first when the release is newer than it, and `--dry-run` lists the edits.
  `giong upgrade --self` updates the command the way it was installed (the install script's
  archive with its SHA-256, or `cargo install`). Codes `GIONG-E5093` to `GIONG-E5098`.
- **giong-cli**: `giong doctor` reads the project's `Cargo.lock`: giong crates on different
  releases (`GIONG-E5091`), a newer giong release out (`GIONG-E5090`, asked of crates.io
  through `cargo info` at most once a day; `--offline` skips it, `GIONG_LATEST` answers it)
  and the command older than it (`GIONG-E5092`).
