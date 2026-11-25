### Fixed

- **giong-cli**: the curl lines of `giong routes`, `giong openapi` (`x-codeSamples`) and the
  dashboard quote a method that holds more than letters, digits, `-` and `_`. HTTP allows
  `|`, `&`, `$` and a backquote in a method, and the method comes from the service, which
  `--mcp` can point at someone else's: a pasted line ran what the method held.
