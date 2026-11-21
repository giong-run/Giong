### Added

- **giong-postgres: batched reads** (`Config::batch_reads`, off by default). A pool sends
  the `SELECT`s waiting for one shared connection under one `Sync`, at most 16, never
  waiting for more, behind a prepared statement that makes their transaction read-only.
  The server replies and wakes the client once per batch instead of once per statement: with
  the database as the limit, 3.4 times the one-row lookups (198.3k against 58.7k a second
  on one server core, p99 0.56 ms against 2.0 ms), 18% more on three cores, nothing for
  100-row reads, and little on the full path of an HTTP request (`bench/db-path.sh`, new),
  where the HTTP side is the limit. A `SELECT` that writes fails in the batch with `25006`
  and is sent again alone; statements the server skipped after an error in their batch are
  sent again one by one, as are the other statements of a batch that a failure or a failed
  commit rolled back. A `SELECT` that calls `set_config` runs alone, so that a
  transaction-local setting does not reach other tasks' statements; not for functions that
  make such settings (row-level security set up per request), which a statement's text does
  not show. A batch holds the locks its statements took until it ends, so a statement of a
  batch gives up a lock wait within 50 ms, and one that fails in a batch with a lock
  timeout, a deadlock or a serialization failure is sent again alone: a batch cannot
  deadlock with a transaction that its statements one by one would not (a migration
  locking two tables). MySQL's protocol has no equivalent.
- `bench/db-path.sh`: an HTTP request that reads one row from PostgreSQL or MySQL, answered
  as JSON, on giong against axum with deadpool-postgres and with SQLx, for throughput,
  p50/p99 under load and the latency of one request at a time.
