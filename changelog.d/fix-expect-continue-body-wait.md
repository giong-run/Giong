### Fixed

- **giong-http**: over HTTP/2 and HTTP/3, a request body sent with `expect: 100-continue`
  is no longer cut while the handler works before reading it. The wait for the body (the
  request timeout, and `Server::min_body_rate` when it is on) now starts when the handler
  asks for the body, which is when `100 Continue` goes out, as over HTTP/1.1. Before, a
  handler that spent longer than the timeout or the rate's grace period before reading (an
  authorization check, say) got `GIONG-E1112`, because the client was still waiting for
  `100 Continue` ([#492](https://github.com/giong-run/Giong/issues/492)).
