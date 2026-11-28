### Fixed

- **giong-cli**: `giong cache share` and `giong cache fill` now give a project they skipped
  the fix for its cause. A build directory or store that could not be written was told to
  `rustup update stable` by `share`, and anything but a running build was told to check
  `cargo generate-lockfile` by `fill`. `fill` also reports a store that cannot link or write
  as a warning, as `share` does.
