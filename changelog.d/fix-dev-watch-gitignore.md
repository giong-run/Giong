### Fixed

- **giong-cli**: `giong dev` no longer restarts in a loop in a project made with `giong new`
  (SQLite, the default): the service's writes to `data/app.db-wal` and `data/app.db-shm`
  counted as a change, and each restart wrote them again, about once a second (63 restarts
  in a minute; 1 now). The watcher skips what the project's `.gitignore` ignores, except
  `.env`, which still restarts the service, and never watches SQLite files (`*.db`,
  `*.sqlite`, `*.sqlite3` and their `-wal`, `-shm` and `-journal` files)
  ([#514](https://github.com/giong-run/Giong/issues/514)).
