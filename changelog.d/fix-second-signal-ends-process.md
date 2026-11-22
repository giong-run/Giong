### Fixed

- **giong**: with `Builder::shutdown_on_signals(true)`, a second SIGINT or SIGTERM now ends
  the process at once, as it would with no handler; every signal after the first used to be
  ignored, so a shutdown that hung could be stopped only with `kill -9`. A signal within
  250 ms of the first counts as the same request, since wrappers such as cargo-watch
  forward the Ctrl-C their process group also got
  ([#271](https://github.com/giong-run/Giong/issues/271)).
