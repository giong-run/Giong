### Fixed

- **giong-cli**: `giong routes` and `giong openapi` work on a web project from `giong new`. The
  template depended on giong-mcp without its `web` feature, which adds the routes tool they
  ask, so both failed on every new project with `GIONG-E5058: the service's giong-mcp has no
  routes tool`. The project already depends on giong-web, so the feature adds nothing to build.
