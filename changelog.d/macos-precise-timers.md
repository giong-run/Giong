### Added

- **giong**: `Builder::precise_timers(true)` makes short timed waits on macOS end on time
  instead of late by the timer slack macOS adds to `kevent`'s timeout (a 2 ms sleep took 2.0
  to 3.8 ms on CI's machines). For waits up to 100 ms the kqueue driver then arms a one-shot
  `EVFILT_TIMER` with `NOTE_CRITICAL` in the same `kevent` call, which the kernel fires on
  time. It is off by default because it costs power: a task sleeping 2 ms in a loop used
  7.05 instead of 5.28 ms of CPU per second, with 411 instead of 348 interrupt wake-ups per
  second. Linux and Windows ignore it ([#281](https://github.com/giong-run/Giong/issues/281)).
