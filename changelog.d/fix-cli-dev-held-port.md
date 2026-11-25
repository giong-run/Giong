### Fixed

- **giong-cli**: `giong dev` without a ready signal (Windows, a giong older than 1.5, a
  service without giong) no longer takes another process accepting on the service's port
  for the new build having started. A build that then failed to bind it was reported as
  serving and then as a later crash; it is now a failed start, whose finding names who
  holds the port.
