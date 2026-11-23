### Added

- **giong**: Unix domain sockets on Windows. `UnixListener` and `UnixStream` work there
  (Windows 10 version 1803 and later, Server 2019 and later) through Winsock's `AF_UNIX`,
  connected and accepted through the completion port like TCP. `UnixStream::pair` connects
  two sockets through a short-lived listener in the temporary directory, since Windows has
  no `socketpair`. Three codes say what Windows cannot do: `GIONG-E0060`
  (`Code::UnixSocketInUse`, a file is already at the path: giong never removes it),
  `GIONG-E0061` (`Code::UnixPathTooLong`, over the 107 bytes Winsock takes) and
  `GIONG-E0062` (`Code::UnixSocketsUnavailable`, a Windows without them).
- **giong**: `net::UnixSocketAddr`, the address `local_addr` and `peer_addr` of Unix sockets
  return: the standard library's Unix `SocketAddr` on Unix, a type with the same
  `as_pathname` and `is_unnamed` on Windows, whose standard library has none.
- **giong**: the `unstable-udp-offload` feature builds on Windows. `send_to_with` and
  `send_segments_to` split a send into datagrams with `UDP_SEND_MSG_SIZE` where Windows has
  it (`gso_supported` says; Windows 10 22H2 does), `set_gro` turns on receive coalescing
  (URO) on the versions that have it, `set_recv_ecn` and `SendOptions::ecn` read and write
  the ECN bits (`IP_ECN`), `set_recv_local_addr` and `SendOptions::source` use `IP_PKTINFO`, and
  `set_dont_fragment` sets "don't fragment" with the path MTU left to the caller. Datagrams
  that a shared port's thread hands to the workers' sockets carry their control messages
  too. Windows refuses ECN 3 (CE) on a send with `WSAEINVAL`, and where it has no ECN the
  bits are left out of the datagrams sent.
- **giong-tls**: Windows. `TlsConnector::with_system_roots` reads the roots Windows trusts
  for TLS servers from its `ROOT` certificate store, without those in `Disallowed`. Kernel
  TLS stays Linux's: `ktls_support()` says "Windows has no kernel TLS", so `Ktls::Auto`
  keeps connections in the process and `Ktls::Required` fails with `GIONG-E2009`.
- **giong-http**, **giong-web**: Windows. `Server::min_free_memory` reads the memory left
  from the machine's available memory and, in a job object with a memory limit (a Windows
  container), from that limit.
- **giong-mcp**: Windows. The process figures come from the process's own counters
  (working set, handles, threads, start time). A Unix socket there needs a token, as TCP
  does, since giong-mcp cannot restrict the socket file to one user on Windows.
- **giong-compat**, **giong-postgres**, **giong-mysql**, **giong-redis**, **giong-grpc**,
  **giong-quic**: Windows. A database host given as a Windows path (`C:\run\pg`) is a Unix
  socket, as a path starting with `/` is elsewhere. giong-quic runs without ECN on a Windows
  that has none, as QUIC allows.

### Changed

- **giong**: on Windows a TCP or Unix stream is non-blocking from the start, as on macOS,
  so a send made outside the driver (giong-compat's, giong-tls') never holds the worker on
  an accepted socket that has not met the driver yet.

### Fixed

- **giong-compat**, **giong-tls**: on Windows a send they make outside giong's driver hands
  Winsock at most 1 MiB, as giong's own stream sends do (#360): Winsock takes the whole of
  a send while the socket's buffer has room, and would hold all of it for a peer that has
  stopped reading.
- **giong**: on Windows, `TcpStream::shutdown` and `UnixStream::shutdown` of the read half
  end a `recv` or `read` that waits, with the end of the stream, as on Unix; Windows left
  it waiting until the peer closed (giong-redis' client kept a task and a socket for each
  dropped connection to a server that kept its side open).
