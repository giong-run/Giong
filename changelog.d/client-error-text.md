### Fixed

- **giong-http**: a client error for a request not sent again on a reused connection
  ("the connection had been used before...") names its code and link once, and keeps its
  `io::Error` source, so `io::Error::from(e).kind()` is `ConnectionReset` again when the
  server reset the connection ([#235](https://github.com/giong-run/Giong/issues/235)).
