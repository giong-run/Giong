### Fixed

- **giong-grpc**: `Metadata::add` refuses `keep-alive`, `proxy-connection`,
  `transfer-encoding` and `upgrade` with `GIONG-E4101`, as it already refused `connection`.
  HTTP/2 forbids these fields, so they could never be sent: a client call failed at send
  time with `GIONG-E4103`, whose fix text blamed the other end, and a server dropped them
  without a word ([#522](https://github.com/giong-run/Giong/issues/522)).
