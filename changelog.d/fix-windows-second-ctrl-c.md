### Fixed

- **giong**: on Windows, with `Builder::shutdown_on_signals(true)`, a second Ctrl+C or
  Ctrl+Break now ends the process at once, as it would with no handler; every event after
  the first used to be swallowed, so a shutdown that hung could not be stopped from the
  console. An event within 250 ms of the first, such as the same press passed on by a
  wrapper, still counts as the first ([#274](https://github.com/giong-run/Giong/issues/274)).
