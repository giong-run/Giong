### Changed

- **giong-quic** (performance): an endpoint's driver sends quinn-proto's datagrams from the
  buffer they were built in, instead of copying each batch into another buffer; sends a batch
  in its own turn when the socket has room, instead of spawning a task for every batch (only
  a send that has to wait becomes a task); and waits for its connections' next deadline on
  one alarm kept armed from one wait to the next, instead of a new timer every turn. A stream
  round trip costs the server 7.4% fewer instructions (28,308 instead of 30,567; quinn 0.11
  takes 29,354), and under load a request costs 21% fewer syscalls (0.48 instead of 0.61;
  quinn 0.58). A 16 MiB answer costs 5.9% less CPU per KiB on a 4-vCPU VM; quinn's
  zero-copy `write_chunk` still sends it with 21% less. HTTP/3 in giong-http runs on it.
  Method and raw data: `bench/results/quic-send-path/`.
