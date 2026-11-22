### Added

- **giong-cli**: `giong record` turns requests recorded by giong-web's `Record` middleware into
  tests. `list` and `show` read the exchanges in the service's log (`giong dev` keeps it in
  `.giong/dev.log`); `save` writes chosen ones to `tests/recorded/<name>.json` as cases, with
  dates, UUIDs, tokens and ids the service made masked, ids that later requests use captured
  and reused, and redacted headers read from `GIONG_RECORD_<HEADER>`; `replay` sends the cases
  in order to the running service and checks each answer (status, `Content-Type`, `Location`,
  body field by field), exit status 1 on a difference, so it runs in CI. Codes
  `GIONG-E5065` to `GIONG-E5069`.
