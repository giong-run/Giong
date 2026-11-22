### Fixed

- **giong-http**: the HTTP/1.1 client returns the answer a server sent before it read the
  whole request body, such as a 413 or a 401 followed by a close, instead of
  `GIONG-E1106` "sending the request failed", where the system kept that answer readable
  after the server's reset (Linux does; Windows and some BSD timings discard it). It
  already did so for streamed bodies; a buffered body now gets the same. A 408 on a
  kept-alive connection is still taken as the server closing it while idle, and a safe
  request is sent again on a new connection
  ([#232](https://github.com/giong-run/Giong/issues/232)).
