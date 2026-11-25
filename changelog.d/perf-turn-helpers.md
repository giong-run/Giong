### Changed

- **giong**: each turn of a worker's event loop no longer calls out of line the checks that
  almost always find nothing to do (deferred closes, timers, the io_uring completion queue,
  the empty list of finished resources): `yield_now` costs 25.6% fewer instructions on
  io_uring and 14.6% fewer on epoll, `spawn` 18.3% and 9.5% fewer, a small TCP echo about
  4% fewer ([#288](https://github.com/giong-run/Giong/issues/288)).
