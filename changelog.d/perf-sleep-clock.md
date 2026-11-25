### Changed

- **giong**: a worker that goes to sleep reads the clock for `Handle::pulse` from
  the clock `Instant` reads (`CLOCK_MONOTONIC`, `CLOCK_UPTIME_RAW` on macOS) directly
  instead of through `Instant`, which saves about 250
  instructions per sleep: a small TCP echo costs 7.2% fewer instructions on io_uring and
  5.7% fewer on epoll, an HTTP/1.1 request about 2.9% and 2.6% fewer (Unix; Windows is
  unchanged) ([#402](https://github.com/giong-run/Giong/pull/402)).
