### Fixed

- **giong-http**: over HTTP/1.1, the body bytes that arrive with the request head now count
  toward `Server::min_body_rate`, as they already did over HTTP/2 and HTTP/3. A client that
  sent the head and the start of the body in one write got only the grace period before its
  next piece, not the grace plus what those bytes earn, and was cut with `GIONG-E1112` while
  keeping the rate. With `min_body_rate(1000, 200 ms)`, 1,000 bytes sent with the head then
  a 600 ms pause got 400 before and 200 now
  ([#525](https://github.com/giong-run/Giong/issues/525)).
