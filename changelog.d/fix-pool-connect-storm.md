### Fixed

- **giong-postgres**, **giong-mysql**, **giong-redis**: a pool no longer opens a connection
  attempt for every call while its server is down. After a failed attempt, the pool
  opens one connection at a time until one succeeds, and tasks that waited through a
  failed attempt, with no connection open to wait for, get its error at once. With no
  connection open, calls after a failure also get its error at once for a pause of 10 ms
  that doubles with each failure in a row up to 1 s, less a random part of up to half.
  Measured against a closed port with a pool of 4: 64 tasks calling for 300 ms make 6
  attempts for about 9,600 calls in each crate, where every call made its own attempt
  before (9,004 of 9,004 with MySQL); one task calling every millisecond for 400 ms makes
  6 attempts for 200 calls, where it made 200. The error is the server's own, as before,
  and the first success clears the state ([#247](https://github.com/giong-run/Giong/issues/247)).
