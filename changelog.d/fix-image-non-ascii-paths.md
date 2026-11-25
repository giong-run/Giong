### Fixed

- **giong-cli**: `giong image --add` no longer panics on a file whose path is longer than 100
  bytes and has names that are not ASCII (Vietnamese, Chinese, Japanese...): the path was cut
  at a byte count to fit the tar header, which could fall inside a character. A file name that
  is not UTF-8 is refused with `GIONG-E5073` instead of being renamed in the image, where the
  service would not find it.
