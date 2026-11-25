### Changed

- **giong**: a worker that goes to sleep reads the clock for `Handle::pulse` from
  `CLOCK_MONOTONIC` directly instead of through `Instant`, which saves about 280
  instructions per sleep: a small TCP echo costs 7.9% fewer instructions on io_uring and
  6.2% fewer on epoll, an HTTP/1.1 request about 3% fewer (Unix; Windows is unchanged)
  ([#402](https://github.com/giong-run/Giong/pull/402)).
