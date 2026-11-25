### Fixed

- **giong-cli**: the tests that `giong new` writes for a web service with SQLite give each
  test its own database file. The name came from the clock alone. On macOS, where the clock
  ticks in microseconds, two tests starting at once could share one file, and the first to
  finish deleted it under the other.
