### Fixed

- **giong-cli**: `giong new rusqlite` is refused with `GIONG-E5030`, as `giong new serde` is.
  The project it made depends on the `rusqlite` crate (SQLite is the default database), so
  its own crate of the same name did not build: `error[E0464]: multiple candidates for
  rmeta dependency rusqlite`.
