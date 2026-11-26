### Changed

- **giong-cli**: `giong new` no longer warns on Windows that the project's crates do not build
  there (`GIONG-E5034`, "use WSL 2"). Every crate builds and passes its tests on Windows since
  1.5, and CI now builds, tests and starts the projects `giong new` writes on Windows (those
  that need no database server). `giong doctor` drops its matching note, and its JSON key
  `windows_runtime_only` is now `false`.
