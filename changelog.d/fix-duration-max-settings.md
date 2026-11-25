### Fixed

- **giong**: `Builder::spin_before_park(Duration::MAX)` no longer panics the first time a
  worker is idle; a spin longer than a century counts as a century
  ([#298](https://github.com/giong-run/Giong/issues/298)).
- **giong-postgres**, **giong-mysql**: `Pool::wait_timeout(Duration::MAX)` no longer panics
  on the first `get`; a wait longer than a century counts as a century (no limit)
  ([#298](https://github.com/giong-run/Giong/issues/298)).
- **giong-mysql**: `Config::statement_timeout(Duration::MAX)` is sent as the longest each
  server takes (a year; about 49 days for MySQL's `max_execution_time`) instead of a number
  no server accepts, which made every connection fail
  ([#298](https://github.com/giong-run/Giong/issues/298)).
