### Changed

- **giong-quic**: an endpoint finds the connections whose timers are due in a heap, instead
  of asking every connection it holds for its next deadline twice at each wake of its
  driver. A busy connection no longer pays for the idle ones next to it: with one worker
  holding a server and a client endpoint, a stream round trip took 16 µs of CPU with no
  other connection, 104 µs with 1,000 idle ones, 513 µs with 3,000 and 3.5 ms with 10,000;
  it now takes 11–13 µs in all four cases. HTTP/3 in giong-http runs on it
  ([#264](https://github.com/giong-run/Giong/issues/264)).
