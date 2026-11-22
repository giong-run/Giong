### Added

- **giong-web**: `App::routes()` lists what a built app serves (method, full pattern,
  parameters, catch-all, streaming body limit, middleware count, the line that added the
  route), with `RouteList::to_json()` for tools and `RouteList::live()` for the apps alive
  on the current worker. Made once at `build`; nothing runs on the request path.
- **giong-web**: `middleware::Record`, for development: one `exchange` JSON line per
  answered request with headers and bodies (secret headers redacted), the input of
  `giong test record`. Opt-in; code that does not add it pays nothing.
- **giong-mcp**: a `routes` tool behind the new `web` feature: the routes of the giong-web
  apps the workers serve, workers with the same routes grouped.
