### Security

- **giong-web**: on Windows, `Req::param` also refuses a path parameter whose name ends in
  `.` or a space, which Windows drops, so that `a/.env.` or `secret.txt ` opened `a/.env`
  or `secret.txt` past a handler's own checks on the name (and `a/...` opened `a`); a name
  with `~` and a digit before its extension, which Windows may read as the short 8.3 name
  of another file (`SECRE~1.TXT` for `secret-keys.txt`), past the same checks; and the
  device names `COM0`, `LPT0` and those with superscript digits (`COM¹`, `LPT²`...). None
  of these let a value leave the directory. A value with a `:`, such as a time or a URN,
  is refused on Windows since the fix for drives and streams: read it with
  `Req::param_raw` ([#243](https://github.com/giong-run/Giong/pull/243) review).
