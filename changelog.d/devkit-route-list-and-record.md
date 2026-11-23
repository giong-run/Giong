### Added

- **giong-web**: `App::routes()` lists what a built app serves (method, full pattern,
  parameters, catch-all, streaming body limit, middleware count, the line that added the
  route), with `RouteList::to_json()` for tools and `RouteList::live()` for the apps alive
  on the current worker. Made once at `build`; nothing runs on the request path.
- **giong-web**: `middleware::Record`, for development: one `exchange` JSON line per
  answered request with headers and bodies, the input of `giong test record`. Secret
  headers, and query parameters, form fields and JSON keys whose names say secret
  (`password`, `token`, `api_key`, `card`, `cvv`, ...), are written as `[redacted]`;
  `redact_field` and `keep_field` adjust the names. Opt-in; code that does not add it
  pays nothing. A release build has it only with giong-web's new `dev` feature: without
  it the recording code is left out and `Record::new` panics with `GIONG-E1213`.
- **giong-mcp**: a `routes` tool behind the new `web` feature: the routes of the giong-web
  apps the workers serve, workers with the same routes grouped.
