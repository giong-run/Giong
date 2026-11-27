### Added

- **giong-cli**: a build store, so the disk holds one copy of each dependency build for all
  your projects instead of one per project. After each build, `giong dev` replaces the files
  a registry or git dependency's build wrote with hard links to one copy in
  `~/.giong/store` when the bytes are the same; each project keeps its own `target/`, so
  cargo works as before (no shared lock, no binaries overwriting each other, and cargo sees
  every build as fresh: 0 units rebuilt). Four projects from `giong new`: 645.8 MiB of builds
  on disk became 401.3 MiB (-37.9%), in 0.3 s. `giong cache share ~/code` does the same for
  projects already built, `giong cache gc` removes what no project uses and keeps the builds
  under a budget (`GIONG_STORE_BUDGET`, default 30G), and `giong cache` says what it saves.
  New code `GIONG-E5025` (`StoreUnusable`).
