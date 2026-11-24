### Fixed

- **giong-http**: the HTTP/3 client accepts at most 32 informational (1xx) responses before
  the final one, as its HTTP/1.1 and HTTP/2 clients do; a server that sends only 1xx
  answers now fails the request as a protocol error at once, instead of holding it until
  the request timeout.
