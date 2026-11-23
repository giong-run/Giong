### Fixed

- **giong-mysql**: the pool's own methods (`query`, `execute` and the like) refuse `CALL`,
  as they refuse `SET` and `BEGIN`: a stored procedure may change the session, start a
  transaction or take a lock without the server saying so, and the statements of other
  tasks sharing the connection would run in it. Call procedures on a connection from
  `pool.get()` ([#287](https://github.com/giong-run/Giong/issues/287)).
