### Fixed

- **giong-cli**: `giong db migrate` on PostgreSQL reports a failed migration at a line inside
  the file. psql names the line where the failing statement ends, so a last statement
  without its `;` was reported one line past the end of the file, and one with an unclosed
  parenthesis several lines past it (psql reads on to the end of the script).
