### Security

- **giong-quic**: connections no longer offer QUIC DATAGRAM frames (RFC 9221), which
  giong-quic has no API to read. A peer could park up to 1.25 MB of them on every
  connection, and each one held the whole packet it came in. One HTTP/3 client sending a
  one-byte datagram per padded packet made the server hold over 40 MB. A DATAGRAM frame
  now closes the connection with `PROTOCOL_VIOLATION`
  ([#341](https://github.com/giong-run/Giong/issues/341)).
