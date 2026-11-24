### Changed

- **giong-http**: a request from the client to the server costs a quarter fewer CPU
  instructions, both ends counted, over HTTP/1.1 and HTTP/2 (`bench/instr`, io_uring:
  `http1-client` 30,851 to 22,570, `h2` 43,481 to 31,631, `h2-streams` 372,328 to 253,593,
  `https` 42,927 to 34,649; heap allocations per request 33 to 13 over HTTP/1.1 and 48 to
  29 over HTTP/2). The client no longer formats the URL and the `Host` header with `format!` for every request (four
  times each), and looks the route's pool up once instead of twice; the HPACK encoder finds
  a name in the static table with one `match` instead of comparing it with all 61 entries;
  the HTTP/2 and HTTP/3 servers read a response's header fields without a UTF-8 check and
  substring searches; the HTTP/2 client keys its streams with a cheap hash of the ids it
  picks itself instead of SipHash; header names are checked against a table; and a
  response's first header field is written into a buffer allocated once instead of grown
  three times. The HTTP/1.1 server alone costs 8% less (`http1` 9,421 to 8,642, heap
  allocations per request 3 to 1), and a giong-web route 5% less (`web` 13,414 to 12,711, allocations 5 to 3).
