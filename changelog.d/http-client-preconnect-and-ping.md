### Added

- **giong-http**: `Client::preconnect(url)` opens a connection to an origin before the first
  request needs it (DNS, TCP, the proxy tunnel and the TLS handshake, with the client's
  settings) and returns the version it speaks, without sending a request. Requests made
  while it is being opened wait for it and share it once ALPN picks HTTP/2; when ALPN picks
  HTTP/1.1 the connection goes among the idle ones and the waiting requests open their own;
  when it fails or the call is dropped, the first request waiting opens one. No request is
  sent twice. Bounded by the client's timeout; cancel safe. Asked for in
  [#154](https://github.com/giong-run/Giong/issues/154) by a service that warms its client up
  while requests arrive ([#184](https://github.com/giong-run/Giong/pull/184)).
- **giong-http**: `Client::ping_idle_http2(url, deadline)` sends a PING on each idle HTTP/2
  connection to an origin and waits up to `deadline` for the acknowledgements; it returns
  how many answered. A connection that answers counts as used (its idle timeout starts
  again). One that does not answer in time and still carries no request is closed at once
  and takes no more requests (`GIONG-E1103`); one that carried a request during the probe
  is left open. A probe in flight keeps the idle timeout from closing the connection, and
  a GOAWAY read with the acknowledgement fails the call (`GIONG-E1106`). Only
  acknowledgements of the client's own PINGs count. The client never pings unless asked.
  ([#184](https://github.com/giong-run/Giong/pull/184))
