### Fixed

- **giong**: on io_uring, peers that keep sending to tasks that stopped reading (an HTTP client
  that pipelines requests and never reads the answers) no longer make each connection hold
  about half a megabyte of heap. Once more than the receive pool's size waits on the heap,
  stream receives take one buffer per read until half of that is gone, and the rest waits in
  the socket under TCP flow control. 128 such connections held 57 MB; now 16 MB
  ([#322](https://github.com/giong-run/Giong/issues/322)). Those one-buffer receives, and
  the ones a large giong-http request body uses, take a whole buffer from the pool, never a
  piece of the packed buffer ring (Linux 6.12 and newer): with kernel TLS on Linux 7.0 a
  single-shot receive there could report a buffer full while the kernel kept it, and two
  receives then wrote into the same buffer. That is what took this change out of 1.4.1.
