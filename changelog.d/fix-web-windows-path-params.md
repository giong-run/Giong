### Security

- **giong-web**: on Windows, `Req::param` refuses path parameters that `Path::join` there would
  take out of the directory: `..\` and `.\` segments (`%5C`), a `\` in a one-segment parameter,
  values that start with `\` or `\\server` (UNC), a `:` (`C:\windows`, `C:x`, `file:stream`),
  `.` or `..` followed by spaces, and device names (`CON`, `NUL`, `COM1`, `nul.txt`...). Before,
  only `/` counted as a separator, so `GET /files/..%5C..%5Cwindows%5Cwin.ini` passed the check
  that documents a catch-all as safe to join to a directory. Nothing changes on Linux and
  macOS, where these are ordinary file names.
