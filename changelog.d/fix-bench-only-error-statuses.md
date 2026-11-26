### Fixed

- **giong-cli**: `giong bench` stops at once with `GIONG-E5063` when the service answers every
  request with an error status, such as a 404 for a path it has no route for, and says to
  give `--path`. `giong bench --against main` in a project from `giong new` asked for `/`,
  which has no route there: it measured how fast the service says 404 through every round
  (the probe even picked the rate from those answers) and printed a comparison of them before
  reporting that 100% of requests failed.
