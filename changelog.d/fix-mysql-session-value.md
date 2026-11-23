### Fixed

- **giong-mysql**: a `Config::session_variable` value must be one SQL expression. A value
  with a `,` outside quotes and parentheses, a `;`, a comment or a backslash is refused when
  connecting: `"'', autocommit = 0"` used to turn off autocommit, which the pool relies on,
  past the check that refuses the variables giong-mysql sets itself
  ([#329](https://github.com/giong-run/Giong/issues/329)).
