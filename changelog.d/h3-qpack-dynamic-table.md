### Added

- **giong-http**: `Server::qpack_table_size` (feature `unstable-http3`) allows HTTP/3 clients a
  QPACK dynamic table (RFC 9204), off by default. A client then sends a field it repeats (a
  cookie, its `user-agent`, the `:authority`) once and refers to it after: with aioquic's
  encoder, 50 requests carrying a 200-byte cookie took 1,168 bytes of field sections and encoder
  stream instead of 12,350 (−90.5%), and the server spent 13% fewer instructions per request. A
  connection holds at most the configured bytes of table, at most 16 requests may wait for
  insertions, a request that gives up waiting is cancelled for the client's encoder, and a
  client that leaves more than 16 KiB of acknowledgments unread has its connection closed
  (`H3_EXCESSIVE_LOAD`).
  Responses still use the static table alone.
- **giong-http**: `ClientBuilder::qpack_table_size` (feature `unstable-http3`) does the same for
  responses: an HTTP/3 server may fill a dynamic table of that size, off by default. Against an
  aioquic server repeating `server`, `cache-control`, a 150-byte `set-cookie` and more, 100
  response heads took 1,737 bytes instead of 23,391 (−92.6%), and the client spent 12% fewer
  instructions per request. Requests still use the static table alone.
- **giong-http**: the `h3_get` example, an HTTP/3 client for trying other servers.
