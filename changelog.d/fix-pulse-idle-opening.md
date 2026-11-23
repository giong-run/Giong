### Fixed

- **giong**: `Handle::pulse` no longer reports more idle time between two readings than passed
  between them. A reading taken while a worker had read the clock for its sleep, but not yet
  published it, missed the sleep, and the next reading then counted the sleep from before the
  first one ([#320](https://github.com/giong-run/Giong/issues/320)).
