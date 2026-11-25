### Fixed

- **giong-cli**: an HTTP answer with a chunk size near the largest number (such as
  `ffffffffffffffff`) no longer panics `giong record replay`, the dashboard's request box or
  the giong-mcp client of `giong top` and `giong routes`: it is reported as a chunk cut short.
  The record and dashboard client also no longer copies a chunked body after every read to see
  whether it is complete.
