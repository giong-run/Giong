### Added

- **giong-http**: `Server::min_body_rate(bytes_per_sec, grace)`, off by default: a request
  body read as a stream that arrives slower on average than the rate once the grace period
  is over is cut with `GIONG-E1112`, over HTTP/1.1, HTTP/2 and HTTP/3. Only the time the
  server waits for the client counts, not the time a handler takes with a piece. Without
  it, `request_timeout` bounds each piece but not the body, so one byte every 29 s held a
  handler and its connection or stream for ever (the slow-POST form of slowloris;
  [#267](https://github.com/giong-run/Giong/issues/267)).
