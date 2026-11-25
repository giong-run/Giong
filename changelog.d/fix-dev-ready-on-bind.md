### Fixed

- **giong-cli**: on Windows (and for a project whose giong is older than 1.5) `giong dev`
  counts a new build as started only once it accepts connections on its `LISTEN` address,
  within `--ready-timeout`, as it already did on Unix. Before, a build that was alive
  300 ms after it started counted as serving, so one that crashed in a slow start-up was
  reported as ready and then as a later crash (`stage: "exited"`), not as a failed start.
