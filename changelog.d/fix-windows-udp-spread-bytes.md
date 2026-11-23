### Security

- **giong**: on Windows, the datagrams waiting for one socket of a UDP port that several
  workers share are now bounded by bytes (4 MiB) as well as by count. 4096 datagrams of up to
  64 KiB each could hold 256 MiB for a slow worker, and a full inbox no longer allocates a
  copy of each datagram it drops ([#324](https://github.com/giong-run/Giong/issues/324)).
