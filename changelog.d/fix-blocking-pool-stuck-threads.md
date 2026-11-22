### Fixed

- **giong**: 64 file operations that never return on blocking threads (opens
  of FIFOs with no writer, calls on a hung network filesystem; every file operation on the
  epoll and kqueue drivers runs there) no longer hold up every other
  file operation of the process. When every blocking thread is busy and none finishes for
  100 ms, one more thread starts, up to 512
  ([#260](https://github.com/giong-run/Giong/issues/260)).
