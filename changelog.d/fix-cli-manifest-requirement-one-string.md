### Fixed

- **giong-cli**: `giong upgrade` reads a dependency's version only when it is one string. In a
  Cargo.toml whose inline table ends early at a stray `}`, a version such as
  `"1.4" }x = ["a"` was taken whole, and the edit replaced the bytes between the strings,
  leaving other values in the file. Found by the manifest fuzz target.
