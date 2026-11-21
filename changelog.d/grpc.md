### Added

- **giong-http: trailer fields.** `Trailers` holds the fields sent after a body. A streamed
  response ends with them through `BodySender::send_trailers` (a last HEADERS frame over
  HTTP/2, after the last chunk over HTTP/1.1), a handler reads a streamed request's with
  `BodyStream::trailers`, and the client reads a response's with
  `StreamingResponse::trailers`. Fields that mean nothing after a body (`content-length`,
  `host`, `set-cookie` and the like) are refused when trailers are built (`GIONG-E1122`,
  `HttpInvalidTrailer`) and dropped when they are received. Until now trailers were
  checked and dropped, which a request body received whole still does; a streamed one's
  trailers count against `max_head_size` over HTTP/2.
- **giong-http client: request bodies written piece by piece.**
  `RequestBuilder::streamed_body` returns a `RequestBody` to send the pieces through, with
  backpressure, `abort` and `send_trailers`. Over HTTP/2 the response is read while the
  request is still being written, so requests and responses can alternate on one stream;
  over HTTP/1.1 the body is sent chunked and the response read after it. Redirects are not
  followed for such a request, and it is sent again after a refusal only if no piece had
  been taken.

- **giong-http: `BodySender::closed`** waits until the client of a streamed response is
  gone (it reset the stream or its connection closed), for a handler that produces a
  response on its own and should stop when nobody reads it.
- **giong-grpc** (new crate): gRPC servers and clients on giong-http's HTTP/2. The four
  kinds of call (unary, server streaming, client streaming, bidirectional), deadlines sent
  and enforced on both sides, cancellation that drops the server's handler, text and
  binary metadata, statuses with details, gzip per message, message size limits (4 MiB by
  default), TLS through ALPN. A streaming response's headers go out with its first message, or as
  a Trailers-Only response when the handler ends without sending one, as grpc-go and tonic
  do: metadata set before the first message still arrives first. `giong_grpc::Server` is a giong-http handler; a `Channel`
  shares HTTP/2 connections between calls. The statuses it raises itself carry
  `GIONG-E4101` to `GIONG-E4110` with the cause and the fix. Tested against grpc-go in
  both directions with the cases of gRPC's interoperability specification, on io_uring and
  epoll (`giong-grpc/interop/run.sh`, the `gRPC` workflow), and by hand against tonic
  0.14.2's interop client and server (the eleven cases they implement). Measured against tonic 0.14
  with `bench/grpc.sh` (4-core cloud VM, median of 10 rounds): a one-thread server does 30%
  to 50% more 100-byte unary calls a second, 21% to 25% more 10,000-byte ones, 48% to 50%
  more round trips on bidirectional streams and 3.8 to 4.0 times the messages of a server
  stream, and holds 5.7 kB instead of 11.6 kB per open stream. A one-thread client is level
  to 14% ahead (round trips against tonic's server, which that row measures: −1.7%). Tables and
  what was not measured: `giong-grpc/README.md`; raw data: `bench/results/grpc/`.
- **giong-grpc: health checking and server reflection.** `giong_grpc::health::Health`
  serves `grpc.health.v1.Health` (Check, List, Watch) from statuses the program sets from
  any thread, with `check` and `watch` for clients; `giong_grpc::reflection::Reflection`
  serves `grpc.reflection.v1` and `v1alpha`, so `grpcurl` lists, describes and calls a
  server's methods without its `.proto` files.
- **giong-proto** (new crate): Protocol Buffers decoded as views over the receive buffer,
  with no copy and no allocation for strings, bytes, repeated fields and maps, and message
  structs whose fields borrow for encoding. Passes Google's protobuf conformance suite and
  is fuzzed against prost. Codes `GIONG-E4001` to `GIONG-E4008`.
- **giong-grpc-build** (new crate): generates the giong-proto messages and, for each
  service, a trait to implement, a server that routes calls to it and a client, from
  `.proto` files at build time. The files are parsed in Rust (protox): no `protoc` to
  install. The generated file also carries the descriptors reflection serves.

### Changed

- **giong-http client: HTTP/2 requests sent with `send_streaming` carry `te: trailers`**,
  since their trailers can now be read; servers that only send them to a client that asks
  (gRPC) do so. Requests sent with `send` are as before.
- **giong-http client** (performance): a request costs 1.0% to 1.4% more instructions than
  in 1.0.0-rc.1 (`bench/instr`: `http1-client` 30,512 to 30,939, `h2` 43,059 to 43,563,
  `https` 42,596 to 43,175 on io_uring), the price of request bodies that can be streamed;
  allocations and the memory of idle connections are unchanged. The baseline is updated.
- **giong-http** (performance): the HTTP/2 server puts the small pieces a handler sends one
  after the other through `BodySender::send` into one DATA frame (up to 16 KiB) when they
  are ready in the same turn of the connection, and lets handlers that were woken while it
  polled them go on for up to 32 rounds before it writes, instead of a frame and a write
  for each piece. `send` waits for the connection to take each piece as before.
- **giong-http** (performance): the HTTP/2 server sends the answers to a burst of short
  requests in one write: answers whose bytes fit in one DATA frame fill the output up to
  768 KiB, where it stopped at 64 KiB, and with one write in flight a connection waited a
  turn of the worker for each further write. Streamed and file bodies still stop at 64 KiB.
  The bytes in the output now count against the connection's budget for unsent responses
  until they are written, so a connection holds no more than before. 10,000-byte gRPC unary
  calls from tonic's client on one server thread (`bench/grpc.sh`, 4-core cloud VM,
  io_uring): 62,597 to 89,862 calls a second with 16 in flight per connection.
- **giong-http client** (performance): over HTTP/2, `StreamingResponse::chunk` hands over a
  received DATA frame's buffer as it is (small frames still arrive joined) instead of
  copying the frames that wait into one piece. Pieces stay within 64 KiB as documented.

### Fixed

- **giong-http**: on an HTTP/2 connection with more streamed request bodies open at once
  than the body budget has full windows for (from the fourth, with the default budget of
  1 MiB), the streams after the first few were never given flow-control window again once they had
  used the window they started with, although their handlers had read everything: they
  stalled until an older stream ended, and for good next to streams that never end
  (bidirectional gRPC streams, long uploads read as they arrive). A stream whose handler
  has taken what it sent now gets back the window it started with, which the budget keeps
  for it while it is open; windows beyond that are still granted only within the budget.
  A body received whole before its handler runs holds what it received, so it gets no
  window past the budget. Found with 16 bidirectional gRPC streams on one connection, of
  which three made progress.
- **giong-http**: a timeout of `Duration::MAX` (or anything past what the clock can add)
  given to `Server::idle_timeout`, `request_timeout`, `write_timeout`, `shutdown_linger`,
  `ClientBuilder::timeout`, `connect_timeout`, `idle_timeout` or `RequestBuilder::timeout`
  panicked when it was added to the clock, in the connection's task. Such a timeout now
  counts as a century: no timeout.
