### Fixed

- **giong-postgres**: a pooled connection that goes back to its pool while a `cancel()` from
  its time with the task is still on its way is closed rather than lent to another task,
  whose statement the request would have cancelled. The token's check that the connection
  is still out came before the round trip that sends the request
  ([#283](https://github.com/giong-run/Giong/issues/283)).
