### Fixed

- **giong**: on Windows, timers no longer fire up to 15.6 ms late. The completion port's own
  timeout ends on the system's clock tick, so a 1 ms sleep took 15.6 ms; the driver now
  ends its wait with a high-resolution waitable timer reported to the port
  (`NtAssociateWaitCompletionPacket`), and a 1 ms sleep takes about 1 to 2 ms. Where Windows
  lacks those calls (before Windows 10 1803, or under Wine), the driver keeps the
  millisecond timeout.
