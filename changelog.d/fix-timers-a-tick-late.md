### Fixed

- **giong**: a timer armed right after another one fired (the usual
  `loop { sleep(d).await; ... }`) no longer ends a whole millisecond late: a 1 ms sleep took
  2 ms and a 10 ms one 11 ms, because every deadline was rounded up to the next millisecond
  tick. The timing wheel now keeps each timer's exact deadline and the worker sleeps until
  it: a 1 ms sleep takes about 1.04 ms on io_uring and 1.08 ms on epoll, a 10 ms one about
  10.04 ms ([#228](https://github.com/giong-run/Giong/issues/228)).
