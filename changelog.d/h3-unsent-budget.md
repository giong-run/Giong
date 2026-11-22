### Security

- **giong-http**: `Server::serve_h3` (`unstable-http3`) bounds the response bytes a
  connection holds that the client has not taken, as HTTP/2 does: handlers wait once a
  connection holds the larger of `max_body_size` and 1 MiB. A client that asked for many
  large answers on one connection and gave no flow-control credit made the server hold
  them all until the write timeout: 100 answers of 10 MB on one connection took the
  server from 4 to 959 MiB, now 24 MiB
  ([#231](https://github.com/giong-run/Giong/issues/231)).
