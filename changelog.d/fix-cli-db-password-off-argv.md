### Security

- **giong-cli**: `giong db` no longer puts the password of a `DATABASE_URL` (or
  `REDIS_URL`) on a client's command line, which every user of the machine can read
  (`ps`, `/proc/PID/cmdline`). `psql` gets the URL without its password and the password in
  `PGPASSWORD`; `redis-cli` gets it in `REDISCLI_AUTH`, and the user in `--user`. MySQL
  already used `MYSQL_PWD`.

### Fixed

- **giong-cli**: `giong db` no longer panics on a URL whose `%` comes before a character of
  more than one byte (`p%aé`), and `%+1` is no longer read as an escape.
