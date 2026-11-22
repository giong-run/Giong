### Changed

- **giong-web**: a request costs about 8% fewer instructions (the `web` case of
  `bench/instr`: 13,476 to 12,395 on io_uring, 14,348 to 13,259 on epoll; 7.7% fewer with two
  middleware). `Req` is 328 bytes instead of 416 (the first four path parameters are kept
  inline, more on the heap), `Next::run` starts the next middleware or the handler without
  an async layer of its own, which each moved the request, and a path parameter is cut from
  the request target without finding the path again. Routes find `/` with a byte search.
  ([#331](https://github.com/giong-run/Giong/issues/331)).
- **giong-http**: `Request::path` and `Request::query` find the path and the query with a
  byte search, and look for a scheme and host only in a target that does not start with
  `/` ([#331](https://github.com/giong-run/Giong/issues/331)).
