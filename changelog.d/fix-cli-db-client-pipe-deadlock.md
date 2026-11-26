### Fixed

- **giong-cli**: `giong db migrate` and `giong db reset` no longer hang forever on a long
  migration that prints as it runs (rows from a `SELECT`, `NOTICE` lines). giong wrote the
  whole script to the client before reading its output, so once the client had filled its
  output pipe and giong had filled the input pipe, both waited on each other. The script is now
  written while the output is read.
