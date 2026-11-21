### Fixed

- **giong**: an idle worker on the epoll driver no longer spins at full CPU after a file
  operation. A blocking job that finished while its worker took the results rang the
  worker's bell after the results were gone; the bell stayed readable with nothing to
  take, and every later wait ended at once until the next file operation finished. The
  wait that sees such a ring now resets it.
