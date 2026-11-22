### Changed

- **giong-grpc**: a unary call costs 22% fewer CPU instructions, client and server counted
  together (88,317 to 68,941 on one worker over loopback). The client no longer copies the
  request and response futures each time it awaits them nor boxes the wait for each message,
  formats the `grpc-timeout` value and the URL without `format!`, and lowercases metadata
  names only when they are not lowercase already; the server reads the call's header fields
  in one pass instead of one per field it looks for, and writes the status into the trailers
  without formatting the code or copying the field names.
- **giong-http**: the HTTP/2 client decodes header blocks into a buffer it keeps between
  blocks (one exact allocation for a response head, none for trailers), reuses its write
  buffer, and hands the response its header positions instead of a copy; a buffered response
  keeps its URL inside the request description instead of a second string. `bench/instr`,
  io_uring: `h2` 31,631 to 30,231 instructions and 29 to 22 allocations per request,
  `h2-streams` 253,593 to 241,317, `http1-client` 22,570 to 22,260, `https` 34,649 to 34,337.
  `Request::header` compares names as bytes, without a UTF-8 check of each one.
