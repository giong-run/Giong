### Changed

- **giong-http** (performance): an HTTP/1.1 request costs 8.9% fewer instructions
  (`bench/instr`'s `http1`: 7,439 instead of 8,169 on io_uring, server and raw client; 12.9%
  fewer for the server alone, 5,566 instead of 6,387) and no allocation (one before):
  `Response::json` and `Response::text` keep their constant `Content-Type` line borrowed
  instead of checking it and copying it into a new buffer, a `200 OK` status line is written
  in one copy, and the `Date` header is read from the worker's clock of the current turn of
  its event loop rather than from a clock read of its own (the text still changes with the
  second; it can lag by as long as the turn has run, as nginx's cached time does). The other
  HTTP cases went down too: giong-web's router 6.1%, the client over HTTP/1.1 3.7%, HTTP/2
  1.8% (2.3% for ten requests at once), HTTPS 2.2%, each with one allocation fewer per
  request; no case of `bench/instr` went up. Method and raw data:
  `bench/results/http-response-path/`.
- **giong** (performance): a `ShutdownSignal` (and so `until_shutdown`, which giong-http puts
  around every wait of a keep-alive connection) holds no reference to the runtime: making
  and dropping one touches no reference count shared by the workers, and it is 24 bytes
  instead of 32, which makes an idle HTTP/2 connection 8 bytes smaller.
