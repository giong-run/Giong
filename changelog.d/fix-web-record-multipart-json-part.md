### Fixed

- **giong-web**: `Record` redacts the secret keys inside a JSON part of a
  `multipart/form-data` body (metadata sent beside a file, by its content type or a
  content that starts with `{` or `[`), as it does in a JSON body.
