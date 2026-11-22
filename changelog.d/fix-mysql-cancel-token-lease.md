### Fixed

- **giong-mysql**: a `CancelToken` taken from a pooled connection (`Pool::get`) stops working
  once the connection goes back to the pool: `cancel()` then fails with `ErrorKind::Closed`
  (`GIONG-E3108`) and sends nothing. Before, it ran `KILL QUERY` on whatever the connection
  ran then, a statement of another task, as a late watchdog timer would
  ([#237](https://github.com/giong-run/Giong/issues/237)). giong-postgres already worked this
  way.
