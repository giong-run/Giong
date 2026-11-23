### Fixed

- **giong**: On Windows a UDP receive that waits for a datagram no longer holds a receive
  buffer: it waits with a zero-byte peek and reads into a pool buffer only once the
  datagram has arrived, as on Linux and macOS. 1000 idle sockets with 64 KiB buffers no
  longer commit about 62 MiB
  ([#238](https://github.com/giong-run/Giong/issues/238)).
