### Fixed

- **giong-quic**: `Endpoint::connect` returns the connection when the handshake finished,
  even if the server closed it before the connecting task ran (a server that closes right
  after accepting). It failed with `GIONG-E3602` as if the handshake had not finished, which
  a client slowed by ThreadSanitizer hit in the test suite; the close now shows on the
  connection's first use, as `Connection::handshake` already reported it.
