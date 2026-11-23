### Fixed

- **giong-mysql**: a `CancelToken` taken from a pooled connection (`Pool::get`) stops working
  once the connection goes back to the pool: `cancel()` then fails with `ErrorKind::Closed`
  (`GIONG-E3108`) and sends nothing. A connection that goes back while a `cancel()` from
  its time with the task is still on its way is closed rather than lent to another task.
  Before, a late `cancel()`, as from a watchdog timer, ran `KILL QUERY` on whatever the
  connection ran then, a statement of another task
  ([#237](https://github.com/giong-run/Giong/issues/237)).
