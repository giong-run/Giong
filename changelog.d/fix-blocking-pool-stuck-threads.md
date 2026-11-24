### Fixed

- **giong**: 64 file operations that never return on blocking threads (opens
  of FIFOs with no writer, calls on a hung network filesystem; every file operation on the
  epoll and kqueue drivers runs there) no longer hold up every other
  file operation of the process. When every blocking thread is busy and none finishes for
  100 ms, the pool grows (at most doubling per 100 ms), up to 512 threads
  ([#260](https://github.com/giong-run/Giong/issues/260)).
