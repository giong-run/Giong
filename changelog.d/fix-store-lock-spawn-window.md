### Fixed

- **giong-cli**: the build store no longer takes a project's lock, just released, for a
  running build. A process that `giong dev` or `giong cache` starts from another thread holds
  the lock for a moment, until its program runs, so a first build could skip a project to
  fill from and `giong cache gc` could leave a build directory alone with no build running. A
  lock found taken is now tried again for up to 200 ms; a running cargo build still counts as
  busy ([#535](https://github.com/giong-run/Giong/issues/535)).
