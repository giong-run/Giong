### Added

- **giong-http**: `Server::max_connections_per_ip` (off by default) caps the connections one
  client may hold on a worker: an IPv4 address, or an IPv6 /64 network. A client past it
  has its next connection refused at once, before an idle connection is closed to make
  room for it: `429 Too Many Requests` with `Retry-After: 1` and `GIONG-E1124`
  (`Code::HttpServerAddressLimit`) in the body without TLS, closed before the handshake
  with it, and a line on stderr at most once a minute. Without it one client could take
  every place under `max_connections`, since connections that have not sent a request are
  never closed to make room.
- **giong-http**: `Server::first_request_timeout` (off by default) gives a new connection
  that long, from the accept, to send its first request's head: one that has sent nothing
  is closed, one in the middle of a head gets `408`. Connections that send nothing held
  their place for `idle_timeout` (60 s) and up to `request_timeout` more; Go's
  `ReadHeaderTimeout` and ASP.NET Core's `RequestHeadersTimeout` are the same limit.
