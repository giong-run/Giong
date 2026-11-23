### Security

- **giong-http**: the server answers 400 to a `Host` header that is not a host and a port
  (`user@example.com`, `a/b`, `[::1]evil.com`, a raw non-ASCII name), as RFC 9112 requires,
  and checks `:authority` and the `host` field of HTTP/2 and HTTP/3 requests the same way;
  a request target with a fragment (`/a#b`) is refused too. A proxy in front and a handler
  that builds URLs from `Host` could otherwise read two different hosts or paths
  ([#318](https://github.com/giong-run/Giong/issues/318)).
