### Fixed

- **giong-postgres**, **giong-mysql**, **giong-redis**: a pool no longer opens a connection
  attempt for every call while its server is down. After a failed attempt, the pool
  opens one connection at a time until one succeeds, and tasks that waited through a
  failed attempt, with no connection open to wait for, get its error at once. Measured
  with 64 tasks calling for 300 ms against a closed port with a pool of 4: about 150
  attempts for about 9,500 calls in each crate, where every call made its own attempt
  before (9,004 of 9,004 with MySQL). The error is the server's own, as before, and the
  first success clears the state ([#247](https://github.com/giong-run/Giong/issues/247)).
