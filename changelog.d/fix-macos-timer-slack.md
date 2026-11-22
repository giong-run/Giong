### Fixed

- **giong**: on macOS, a short timed wait of the kqueue driver no longer ends late by the
  timer slack macOS adds to `kevent`'s timeout (a 2 ms sleep took 2.0 to 3.8 ms on CI's
  machines). For waits up to 100 ms the worker arms a one-shot `EVFILT_TIMER` with
  `NOTE_CRITICAL` in the same `kevent` call, which the kernel fires on time; longer waits keep
  the timeout, whose slack is under 1% of them and saves power ([#281](https://github.com/giong-run/Giong/issues/281)).
