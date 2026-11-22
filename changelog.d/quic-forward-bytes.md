### Security

- **giong-quic**: the datagrams a server worker passes to the worker that owns their
  connection are bounded in bytes (4 MiB per worker) as well as in number. A receive with
  GRO is up to 64 KiB, so a worker that fell behind could hold 64 MiB, and the connection
  IDs that steer a datagram are plaintext, so anyone could aim a flood at one worker. A
  datagram past the bound is dropped before it is copied, and a passed datagram is now
  copied once instead of twice. Fixes [#326](https://github.com/giong-run/Giong/issues/326).
