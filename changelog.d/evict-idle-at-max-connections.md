### Changed

- **giong-http**: at `Server::max_connections`, the server closes the connection that has
  been idle longest (HTTP/1.1 between requests, HTTP/2 with no request open) when a new
  client arrives, as nginx does with keep-alive connections, instead of leaving the new
  client in the listen queue until a connection closes on its own, up to `idle_timeout`
  later. A client, or many browsers, holding the limit in idle keep-alive connections no
  longer locks every new client out. A connection receiving or answering a request is never
  closed for this. On Linux and macOS; on Windows the server still waits. It costs an idle
  connection about 12 bytes and a request under 1% more CPU instructions (`bench/instr`).
  Fixes [#266](https://github.com/giong-run/Giong/issues/266).
