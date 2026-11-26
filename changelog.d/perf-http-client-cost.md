### Changed

- **giong-http** (performance): the client does less per request. An HTTP/1.1 request and
  its response cost 7 allocations instead of 11 and 8% fewer instructions (`bench/instr`'s
  `http1-client`, client and server together); over HTTP/2, 14 allocations instead of 22 and
  5% fewer instructions (`h2`), 6% fewer with ten streams in flight (`h2-streams`). The
  pieces: HTTP/2 request fields borrow the request's headers instead of copying them, a
  standard method is not copied, the request's description is made only when it times out,
  the URL's host is shared with the origin its connection is looked up by, a response's
  head and body share one allocation over HTTP/2, the URL is parsed with byte searches, and
  each well-formed header line of a response is read in one pass. No API change; the same
  heads and URLs are refused, with the same messages. Method and raw data:
  `bench/results/http-client-cost/`.
