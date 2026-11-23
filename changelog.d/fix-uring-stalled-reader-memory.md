### Fixed

- **giong**: on io_uring, peers that keep sending to tasks that stopped reading (an HTTP client
  that pipelines requests and never reads the answers) no longer make each connection hold
  about half a megabyte of heap. Once more than the receive pool's size waits on the heap,
  stream receives take one buffer per read until half of that is gone, and the rest waits in
  the socket under TCP flow control. 128 such connections held 57 MB; now 16 MB
  ([#322](https://github.com/giong-run/Giong/issues/322)).
