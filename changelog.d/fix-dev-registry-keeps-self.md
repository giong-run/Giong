### Fixed

- **giong-cli**: a `giong dev` whose new build cannot bind its port (`GIONG-E5014`) stays in
  `giong dev status --all` and `giong dev stop --all`. Naming the process that holds the port
  read the list of running `giong dev` processes, and the runner took its own entry for a
  stale one (a process never matches itself there) and removed it.
