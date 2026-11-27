### Changed

- **giong-http** (performance): the server reads a request line and every well-formed header
  line in one pass over their bytes, with the checks it made one after the other (a token
  method, a visible-ASCII target without `#`, `HTTP/1.1` or `HTTP/1.0`, a token name, a value
  without control bytes, CRLF), and goes through the separate checks only for anything else,
  to say what is wrong with it; it accepts and refuses exactly the heads it did. The server
  alone answers `/json` in 3.9% fewer instructions (4,934 instead of 5,137); `bench/instr`'s
  `http1` 1.8% fewer, giong-web's router 1.9%, the client over HTTP/1.1 1.7%, HTTPS 1.1%.
  Method and raw data: `bench/results/http-parse-head/`.
