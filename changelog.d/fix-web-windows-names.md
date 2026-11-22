### Security

- **giong-web**: on Windows, `Req::param` also refuses a path parameter whose name ends in
  `.` or a space, which Windows drops, so that `a/.env.` or `secret.txt ` opened `a/.env`
  or `secret.txt` past a handler's own checks on the name (and `a/...` opened `a`), and
  the device names with superscript digits (`COM¹`, `LPT²`...). Neither let a value leave
  the directory. A value with a `:`, such as a time or a URN, is refused on Windows since
  the fix for drives and streams: read it with `Req::param_raw`
  ([#243](https://github.com/giong-run/Giong/pull/243) review).
