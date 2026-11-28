### Fixed

- **giong-cli**: the build store no longer takes a project's lock, just released, for a
  running build. A process that `giong dev` or `giong cache` starts from another thread holds
  the lock for a moment, until its program runs, so a first build could skip a project to
  fill from and `giong cache gc` could leave a build directory alone with no build running. A
  lock this process let go of in the last 200 ms and finds taken is now tried again until
  then; a running cargo build's lock is still reported at once
  ([#535](https://github.com/giong-run/Giong/issues/535)).
