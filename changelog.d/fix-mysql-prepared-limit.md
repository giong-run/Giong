### Fixed

- **giong-mysql**: a burst of distinct statements at the server's `max_prepared_stmt_count`
  no longer fails. Once a connection hits the limit (error 1461), its prepares take turns:
  each closes the statements nobody is using and tries again, with a short pause, until
  no prepare on the connection has succeeded for a second. Before, each prepare tried
  only once more, and 2,482 of 3,000 concurrent tasks over 41 statements failed against
  a limit of 8 ([#242](https://github.com/giong-run/Giong/issues/242)).
