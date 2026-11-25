### Fixed

- **giong-cli**: a chunk size written with a sign (`+5`) is refused as not hex by `giong bench`,
  `giong record replay`, the dashboard's request box and the giong-mcp client of `giong top` and
  `giong routes`, instead of being read as 5 (`usize::from_str_radix` takes a leading `+`).
- **giong-web**: the recorder's JSON key unescaping needs four hex digits after `\u`, so
  `\u+041` is no longer read as `A`.
