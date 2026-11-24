### Added

- **giong**: child processes, pipes and signals (a 1.x feature).
  `giong::process::Command` (the standard library's builder) with `spawn`, `status` and
  `output`; `Child::wait`, `try_wait`, `wait_with_output`, `signal` and `kill`. A child is
  watched through a pidfd polled on the worker's ring, so waiting needs no thread and no
  `SIGCHLD` handler, and a signal can never reach a process that reused the child's ID; a
  dropped child is reaped (or killed first, with `kill_on_drop`). Piped stdio is
  `PipeReader` / `PipeWriter`, also from `process::pipe()`, and `process::stdin()`,
  `stdout()` and `stderr()` read and write this process's own streams without blocking the
  worker (never made non-blocking, since the parent shares them). Needs Linux 5.3; Linux
  only for now.
- **giong**: `giong::signal::listen(SignalKind)`: any signal a program may handle, as a
  stream that every listener sees, on any thread, on Linux and macOS; it works together with
  `Builder::shutdown_on_signals`. `GIONG-E0016` for the signals that cannot be listened for.
- **giong**: passing open descriptors between processes over Unix sockets:
  `send_with_fds` / `recv_with_fds` on `UnixStream` and the new `UnixDatagram`. A send
  keeps its own copies of the descriptors until the kernel has read them, and a dropped
  receive closes what it received, so nothing leaks and no wrong file is sent. Linux only
  for now.
  `GIONG-E0017` for too many descriptors, none of the bytes to carry them, or a cut-off
  receive. Also `UnixListener::bind_addr`, `UnixStream::connect_addr` (abstract names),
  `peer_cred`, and `AsFd` for every socket.
- **giong**: socket activation: `net::activated_sockets()` takes the sockets systemd passes
  (`LISTEN_FDS`, `LISTEN_FDNAMES`), and `ActivatedSocket::tcp_listener` (and
  `unix_listener`, `udp_socket`, `unix_datagram`) gives each worker its own listener on
  one; `GIONG-E0018` for a second call or the wrong kind of socket. `GIONG-E0019` when a
  child cannot be watched.
- **examples**: `stdio`, a line protocol over stdin and stdout that runs programs and notes
  `SIGHUP`.

### Fixed

- **giong**: dropping a `TcpListener` whose descriptor was lent out (`as_fd`, `as_raw_fd`)
  no longer shuts the socket down for every copy of it: a listener passed to another
  process, or duplicated, keeps listening.
