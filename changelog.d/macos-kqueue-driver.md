### Added

- **giong**: macOS support: a kqueue driver for the readiness loop (`DriverKind::Kqueue`,
  chosen by default on macOS), with each worker woken through an `EVFILT_USER` event of
  its own queue. The workers of a TCP or UDP address share one socket, bound without
  `SO_REUSEPORT`, so no other process can take the port. Every socket has `SO_NOSIGPIPE`;
  streams are non-blocking from the start (macOS waits in a send to a full socket even with
  `MSG_DONTWAIT`); UDP sockets that giong binds take datagrams of every length UDP allows.
  The whole test suite passes on macOS 15 and macOS 26 (Apple Silicon), as do
  ThreadSanitizer and the soak tests. What still differs from Linux is listed in
  `docs/design/portability.md` ([#173](https://github.com/giong-run/Giong/pull/173)).
- **giong**: the portability layer under the drivers (`src/sys/`, one `Poller` trait for
  epoll and kqueue, a wake-up per driver), on which the macOS and Windows drivers are
  built; no change on Linux ([#173](https://github.com/giong-run/Giong/pull/173)).
- **giong**: `ResolverConfig::system` reads the resolver macOS writes to
  `/etc/resolv.conf` ([#173](https://github.com/giong-run/Giong/pull/173)).
- **giong-tls**: `TlsConnector::with_system_roots` reads the keychain's trust settings on
  macOS ([#173](https://github.com/giong-run/Giong/pull/173)).
- **giong-http**: `Server::min_free_memory` reads the kernel's free and inactive pages on
  macOS ([#173](https://github.com/giong-run/Giong/pull/173)).
- **giong-mcp**: process figures come from `proc_pidinfo` on macOS
  ([#173](https://github.com/giong-run/Giong/pull/173)).

### Fixed

- **giong**: `Builder::build` refuses a driver the operating system does not have:
  io_uring with `GIONG-E0001`, the others with `GIONG-E0004`, whether named through
  `Builder::driver` or `GIONG_DRIVER` ([#173](https://github.com/giong-run/Giong/pull/173)).
- **giong**: `ResolverConfig::parse` (and so `ResolverConfig::system`) keeps a
  `nameserver` with a zone, such as `fe80::1%en0`, as a scoped IPv6 address; it was skipped,
  which could leave only the `127.0.0.1` fallback
  ([#173](https://github.com/giong-run/Giong/pull/173)).
- **giong**: a worker on the epoll (or kqueue) driver no longer spins at full CPU while
  idle when a file operation on the blocking threads finished just as the worker collected
  the results; the stale wake-up is now reset
  ([#173](https://github.com/giong-run/Giong/pull/173)).
