### Changed

- **giong-http**: an HTTP/3 connection now shuts down in two steps, as RFC 9114 (section 5.2)
  recommends: a first GOAWAY that refuses nothing stops new requests, the requests the client
  sent before it heard of it are still taken for `Server::shutdown_linger` (default 100 ms, at
  least a round trip), and then the GOAWAY that names the first request refused. Before, those
  requests got `H3_REQUEST_REJECTED` and had to be retried elsewhere. The cost, stated: an idle
  HTTP/3 connection closes up to `shutdown_linger` later in a shutdown, as an idle HTTP/1.1
  connection already did. `shutdown_linger(Duration::ZERO)` keeps the single GOAWAY.
