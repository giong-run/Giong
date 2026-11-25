### Fixed

- **giong-cli**: the tests that `giong new --template worker` writes give each test its own
  Redis queue. The name came from the clock alone. On macOS, where the clock ticks in
  microseconds, two tests starting at once could share a queue and take each other's jobs.
