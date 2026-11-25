### Fixed

- **giong**: `fs::read` of a file whose length is more than memory holds (a sparse disk
  image or core dump is enough) returns `GIONG-E0204` (`FileTooLarge`, kind `OutOfMemory`)
  instead of aborting the whole process; every allocation it makes is fallible now
  ([#296](https://github.com/giong-run/Giong/issues/296)).
