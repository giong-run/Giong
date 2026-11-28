### Added

- **giong-http**: `Server::serve_h3` schedules responses by RFC 9218 priorities: the urgency of
  a request's `priority` header field, a PRIORITY_UPDATE frame from the client (buffered for up
  to 64 requests not read yet), or the response's own `priority` field when the handler sets
  one. More urgent responses are sent first; responses of equal urgency share the connection in
  turn, so the incremental flag is accepted and not used. PRIORITY_UPDATE on a request stream,
  or for a stream that is not a request stream, is the connection error RFC 9218 names. The
  server's control stream goes ahead of every response. On a connection carrying four 1 MiB
  responses over an 8 MB/s path, a fifth 1 MiB response asked for with `priority: u=0` arrives in
  157 ms instead of 655 ms (−76%); the four others end 0.6% later. Cost: 73 instructions per
  request (+0.12%). See `bench/results/h3-priorities/`.
- **giong-quic**: `SendStream::set_priority` and `Connection::set_priority` (by stream ID): of
  the streams with data to send, those with the highest priority go first.
