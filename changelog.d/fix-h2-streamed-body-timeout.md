### Fixed

- **giong-http**: over HTTP/2, a request body read as a stream is no longer cut after
  `request_timeout` while its pieces keep arriving: the timeout counts from the last piece,
  as over HTTP/1.1 and HTTP/3 and as documented. Every giong-grpc client-streaming or
  bidirectional call longer than 30 s was cut with `GIONG-E1112`
  ([#470](https://github.com/giong-run/Giong/issues/470)).
