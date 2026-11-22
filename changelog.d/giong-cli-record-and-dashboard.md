### Added

- **giong-cli**: `giong record` turns requests recorded by giong-web's `Record` middleware into
  tests. `list` and `show` read the exchanges in the service's log (`giong dev` keeps it in
  `.giong/dev.log`); `save` writes chosen ones to `tests/recorded/<name>.json` as cases, with
  dates, UUIDs, tokens and ids the service made masked, ids that later requests use captured
  and reused, and redacted headers read from `GIONG_RECORD_<HEADER>`; `replay` sends the cases
  in order to the running service and checks each answer (status, `Content-Type`, `Location`,
  body field by field), exit status 1 on a difference, so it runs in CI. Codes
  `GIONG-E5065` to `GIONG-E5069`.
- **giong-cli**: `giong dashboard` (and `giong dev --ui`) serves a local page on
  `http://127.0.0.1:7700` for the service `giong dev` runs: build state, the live request log
  with bodies when `Record` is on, routes with curl lines and an OpenAPI download, workers and
  tasks with a one-second profile, connection pools, warnings with their error pages offline,
  a box to send requests, and "save as test". It reads the service's output and giong-mcp, so
  the service runs no extra code; only this machine can open it, and other web pages cannot
  use it. Codes `GIONG-E5053` and `GIONG-E5054`.
