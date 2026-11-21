### Fixed

- **giong**: on macOS, a stream write or connect that finds the system's network buffers
  used up (`ENOBUFS`) waits for them, as it would on Linux, instead of failing at once: it
  is tried again after 1 ms, then 2, 4, 8, 16 and every 32 ms, for at most 2 s, while the
  worker runs its other tasks. A connect the shortage broke after it began starts over on
  a new socket, and TLS streams wait the same way. After 2 s the error is the new
  `GIONG-E0028` (`Code::NetworkBuffersExhausted`, kind `OutOfMemory`), with the kernel's
  error as its source. Datagram sends keep the kernel's `ENOBUFS`. Nothing changes on
  Linux.
