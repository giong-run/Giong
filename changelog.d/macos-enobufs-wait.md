### Fixed

- **giong**: on macOS, a stream write or connect that finds the system's network buffers
  used up (`ENOBUFS`) waits for them, as it would on Linux, instead of failing at once: it
  is tried again after 1 ms, then 2, 4, 8, 16 and every 32 ms, for at most 2 s, while the
  worker runs its other tasks. A connect the shortage broke after it began starts over on
  a new socket. After 2 s, and at once for a datagram, the error is the new
  `GIONG-E0028` (`Code::NetworkBuffersExhausted`, kind `OutOfMemory`), with the kernel's
  error as its source. Nothing changes on Linux.
