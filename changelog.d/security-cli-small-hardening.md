### Security

- **giong-cli**: the dashboard's send box reaches only this machine, as documented: it
  accepted any host that began with `127.`, so a name such as `127.0.0.1.example.com` was
  looked up in DNS and the request went to another machine. Only an IPv4 loopback address,
  `localhost` or `[::1]` passes now.

### Fixed

- **giong-cli**: a dependency version requirement whose last part is 4294967295 (such as
  `^4294967295`) no longer overflows while `giong` checks the manifest: a panic in a debug
  build and a wrong answer in a release one.
