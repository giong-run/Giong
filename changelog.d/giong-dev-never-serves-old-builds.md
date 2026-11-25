### Changed

- **giong-cli**: `giong dev` never serves code older than the files. A change that does not
  compile, or that compiles and then stops before it serves (a panic at start-up, a port it
  cannot bind, no bind within `--ready-timeout`), now stops the build that was running too and
  closes the port, so a request is refused at once instead of being answered by the old build.
  The cause is shown with its code, the compiler's errors or the service's last lines, and the
  fix: in the terminal, in `giong dev status` (`state` `failed`, and a new `failure` object in
  its JSON and in `.giong/dev.json`) and at the top of the dashboard. The next save builds and
  starts again; a service that stops by itself later is reported the same way and is not
  restarted in a loop. `giong dev status` also says `starting` while a new build starts.

### Fixed

- **giong-cli**: a new build that stopped at start-up while the previous one kept serving was
  reported only in the terminal: `giong dev status` said `serving` and the dashboard `running`,
  so a person or an agent tested the old code without knowing.
- **giong-cli**: after the service stopped by itself, `giong dev` kept its listening socket
  open, so a client waited in a queue nobody took from instead of being refused.
- **giong-cli**: after a failed build, a change to `.env` alone restarted the binary of the
  last build that worked, which is older than the files; it now builds.
- **giong-cli**: a service that panics because its port is in use is reported as
  `GIONG-E5014` naming the process that holds the port, now on Windows too (`netstat`), and
  saying so when that process is `giong dev` itself (the service bound the address with
  another listener than giong's).
- **giong-cli**: `giong dev` no longer binds `LISTEN` for a project that does not depend on
  giong at all (no giong in `Cargo.lock`): nothing in such a service takes the socket, so it
  only kept the service from binding its own port.
