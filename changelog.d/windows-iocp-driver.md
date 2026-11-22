### Added

- **giong**: Windows support. The runtime, TCP, UDP, files, timers, DNS and signals run on
  Windows through a completion-port driver (tested on Windows Server 2022 and 2025),
  `DriverKind::Iocp` (`GIONG_DRIVER=iocp`), the only driver there. Listeners and UDP
  sockets of one address on several workers share one socket, as Windows has no
  `SO_REUSEPORT`: a thread hands connections round to the listeners and datagrams to the
  sockets by the sender's address. Files are read and written overlapped through the port,
  other file operations and consoles go to the blocking threads, and the DNS configuration
  comes from the network adapters. `Builder::shutdown_on_signals` listens for Ctrl+C,
  Ctrl+Break, console close, log-off and shutdown. Not yet on Windows: Unix sockets,
  `unstable-udp-offload` (a compile error), system trust roots, more than 64 CPUs, and the
  crates other than giong ([#174](https://github.com/giong-run/Giong/pull/174)).
