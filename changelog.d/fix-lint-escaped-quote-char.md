### Fixed

- **giong-cli**: giong's own checks in `giong check` read `'\''` (a quote character) to its
  end. They took the escaped quote for the closing one, so in code such as `['\'','"']`
  the next quote opened a string that hid the code after it, and a blocking call there
  (GIONG-E5006) was not reported.
