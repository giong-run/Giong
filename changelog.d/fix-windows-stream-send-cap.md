### Fixed

- **giong**: on Windows a TCP write to a peer that has stopped reading now waits for it, as
  on Linux and macOS. One overlapped send was handed the whole buffer, and Windows takes
  such a send whole while the socket's buffer has room: a 512 MiB `write_all` to a stalled
  peer ended at once, the kernel held every byte, and a write timeout counting from the last
  progress could never fire. Stream sends now go a megabyte at a time (#360).
