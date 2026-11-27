### Changed

- **giong-http** (performance): an idle HTTP/2 connection holds 24% less memory, and one over
  TLS 34% less: 2,518 bytes of heap instead of 3,318 (`bench/instr`'s `idle-h2`), and 6,397
  instead of 9,683 over TLS (the new `idle-h2-tls`); 8,000 idle HTTP/2 connections over TLS
  take 56.1 MiB instead of 81.2. An HTTP/2 connection now goes on in a task of its own once
  the TLS handshake picks `h2` or the HTTP/2 preface arrives, and the task that accepted it
  ends: that task was as large as the HTTP/1.1 state, which an HTTP/2 connection never uses,
  and it stayed beside the HTTP/2 state for as long as the connection lived.
  `Server::serve_connection` still serves its connection to the end in the future it
  returns.
- **giong-tls** (performance): a receive on an established stream no longer holds room for
  the handshake's sends (a key update or an alert goes out without waiting, as it already
  did), so a waiting `TlsStream::recv` future is 136 bytes instead of 640; the rare sends of
  kernel TLS before a write (a key update) are boxed, and the in-process write keeps its
  arguments once: a `TlsStream::write_all` future is 496 bytes instead of 624. An idle HTTPS
  connection takes 5,141 bytes of heap instead of 5,285, and an HTTPS request 2.9% fewer
  instructions (`bench/instr`'s `https`, client and server).
- **giong** (performance): `TcpStream::write_all` and the other stream writes keep their
  arguments once: a write future is 240 bytes instead of 296. Every idle connection of
  giong-http keeps one, so an idle HTTP/1.1 connection takes 1,262 bytes of heap instead of
  1,318. Instructions per operation went down by 0.1 to 1.1% in the other HTTP, TCP and
  database cases of `bench/instr` and are unchanged in the rest; allocations are unchanged
  everywhere. Method and raw data: `bench/results/h2-idle-memory/`.
