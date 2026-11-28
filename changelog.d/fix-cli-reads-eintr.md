### Fixed

- **giong-cli**: on Linux, `giong dev` no longer reports a running service as unreachable, or
  drops a dashboard request, when a process it starts at that moment exits. A child that exits
  sends `SIGCHLD`, which ends a socket read with a timeout early with EINTR when another thread
  is starting a process; the requests to the service, the dashboard's request reader and the
  web client took that for a failure. They now read again within their deadline. In a test
  with 3 threads starting processes, 39 of 40 such reads were ended early
  ([#537](https://github.com/giong-run/Giong/issues/537)).
