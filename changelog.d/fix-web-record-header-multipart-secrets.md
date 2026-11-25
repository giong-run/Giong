### Fixed

- **giong-web**: `Record` redacts the value of every header whose name says secret by the
  rule it already applies to fields (`X-Session-Token`, `Private-Token`, `X-Goog-Api-Key`,
  `X-CSRF-Token`), not only the six it names, and the parts of a `multipart/form-data`
  body (what a browser's `FormData` sends) whose field name says secret. Before, those
  values went into the recording as they were. `keep_field` keeps a header too.
