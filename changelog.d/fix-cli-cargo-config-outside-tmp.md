### Security

- **giong-cli**: `giong bench --against` builds the older commit under `~/.giong/bench`
  (or `$GIONG_HOME/bench`) instead of the temporary directory, and the release check of
  `giong doctor` and `giong upgrade` runs `cargo info` in `~/.giong`. Cargo reads
  `.cargo/config.toml` from every folder above the one it runs in, and on a shared Linux
  machine any user can make `/tmp/.cargo/config.toml`: its `build.rustc-wrapper` ran as the
  user who benchmarked, and a source replacement could name another latest version.
