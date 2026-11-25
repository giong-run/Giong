### Fixed

- **giong-cli**: `giong db` no longer panics on a database URL with a non-ASCII character
  right after a `%` (such as a password `p%€`): the two bytes after the `%` cut the character.
  The `%` is now kept as written, as for any `%` not followed by two hex digits.
- **giong-postgres**, **giong-mysql**, **giong-redis**, **giong-http** (proxy URLs): a `%`
  followed by a sign and a hex digit (`%+5`) is refused as a `%` not followed by two hex digits,
  as the error already said, instead of being read as byte 5 (`u8::from_str_radix` takes a
  leading `+`). The query-name decoding of `giong record` and giong-web's recorder keeps such
  a `%` as written.
