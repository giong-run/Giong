### Fixed

- **giong-http**: once an HTTP/2 client has closed its side of the connection, an answer
  its flow control holds back is reset at once, instead of after `write_timeout` (30 s by
  default): the client can no longer open the window. The handler and the bytes it held
  are freed, and a graceful shutdown no longer waits for such a connection
  ([#263](https://github.com/giong-run/Giong/issues/263)).
