### Security

- **giong-http**: over HTTP/1.1, a 2xx answer to `CONNECT` now closes the connection. The
  client and every proxy on the way take what follows such an answer for a tunnel, which the
  server has none of. A proxy in front that passes `CONNECT` on could then be made to forward
  requests it never looked at, which the server read and answered
  ([#343](https://github.com/giong-run/Giong/issues/343)).
