### Fixed

- **giong-postgres**, **giong-mysql**: a connection URL whose password holds an unescaped
  `/` or `?` no longer has a piece of the password quoted in the error (as the port, a
  parameter or the database name), and no longer reads as a host named after the user
  with the rest of the password as the database name (`postgres://app:12/34@db/x`
  connected to `app:12` and sent `34@db/x` as the database). Such a URL is refused with
  a message that quotes none of it and says to percent-encode the password, as
  giong-redis already did. An `@` in a URL's database name is now refused the same way;
  write it as `%40`
  ([#313](https://github.com/giong-run/Giong/issues/313)).
