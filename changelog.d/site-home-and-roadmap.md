### Added

- **Benchmarks**: build time, disk and binary size of a small service on giong-web against
  axum with tower-http (`bench/build.sh`, [docs/BENCHMARKS.md](docs/BENCHMARKS.md#build-time-and-size)):
  22 crates against 83, a clean debug build in 7.4 s against 18.8 s, a stripped binary of
  1.45 MB against 3.60 MB.

### Changed

- **Roadmap**: the developer kit (the `giong` command) in 1.5.0, then 1.5.x (deep debugging
  on every platform, then every number measured again on bare metal), and 1.6 and 1.7 as
  proposed milestones. giong.run's home page shows each crate head to head with the Tokio
  crate it replaces, with the losses next to the wins, the build numbers and what comes
  next; the roadmap page draws every milestone on one track, gives each one ahead a card,
  and no longer calls a milestone shipped before its release is out.
