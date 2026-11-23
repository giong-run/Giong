### Fixed

- **giong-http**: on HTTP/1.1, `Server::write_timeout` counts from the last progress of a
  write, as on HTTP/2 and HTTP/3, instead of from its start. A response that took longer
  than the timeout to send in all (a 64 MiB body to a client reading at 8 MB/s, with the
  default 30 s) was cut in the middle; it now goes out whole while the client keeps reading.
  A client that stops reading still loses the connection one timeout after the last
  progress. Fixes [#328](https://github.com/giong-run/Giong/issues/328).
