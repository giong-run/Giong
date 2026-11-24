### Added

- **giong**: request tracing, so that a service can say why a request is slow.
  `Handle::set_request_tracing(true)` makes every worker follow each request from its first
  bytes to the last byte of its answer and split its time into phases: receiving it, its
  handler's own polls, waits for pool connections, database commands and other services
  (`giong::observe::wait`, with `Wait::Pool`, `Wait::Query` and `Wait::Upstream`), the rest
  of the handler's time, and writing the answer. Every task poll of 1 ms or more is noted,
  so that a request that waited while another task held its worker names that task, and the
  route of the request whose handler took the poll. Requests are grouped by route
  (`observe::set_route`, `observe::Route`); `observe::take_requests` returns, per route, the
  number of requests and abandoned ones, latency quantiles to within a quarter, where the time
  of any latency band went (`RouteStats::breakdown`), and the slowest eight in full. The
  numbers are kept on each worker, in buckets of fixed size: nothing is sampled away and
  nothing grows with the load. Protocol crates follow requests with
  `observe::RequestTrace`. Off (the default), it costs one read of a thread-local per
  request and per database command
  ([#181](https://github.com/giong-run/Giong/pull/181)).
- **giong-http**: HTTP/1.1 and HTTP/2 servers follow requests while request tracing is on
  (HTTP/2 from the handler's start to its answer); the client times its requests as
  `Wait::Upstream`.
- **giong-web**: requests are named by their route's method and pattern, such as
  `GET /users/{id}`.
- **giong-postgres**, **giong-mysql**, **giong-redis**: pools time the wait for a connection
  (`Wait::Pool`) and clients time each command (`Wait::Query`).
- **giong-mcp**: a `slow_requests` tool. It traces requests for a while (default 5 s), then
  answers per route: requests, p50, p90, p99 and max, where the time of the slowest requests
  went compared with the faster half, a verdict (a waiting pool, slow queries, a slow
  upstream, the handler's own code, a worker blocked by another route or task, a saturated
  worker, slow clients) with the evidence it rests on, a confidence and a suggested fix, and
  the slowest requests in full. It only proposes: nothing in the service changes.
