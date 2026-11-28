### Added

- **giong-http**: `Server::qpack_table_size` (feature `unstable-http3`) allows HTTP/3 clients a
  QPACK dynamic table (RFC 9204), off by default. A client then sends a field it repeats (a
  cookie, its `user-agent`, the `:authority`) once and refers to it after: with aioquic's
  encoder, 50 requests carrying a 200-byte cookie took 1,168 bytes of field sections and encoder
  stream instead of 12,350 (−90.5%), and the server spent 13% fewer instructions per request. A
  connection holds at most the configured bytes of table, at most 16 requests may wait for
  insertions, and a request that gives up waiting is cancelled for the client's encoder.
  Responses still use the static table alone.
