### Added

- **giong**: `TcpStream::recv_into` and `UnixStream::recv_into` are documented, stable API
  (hidden until now; giong-http has received large bodies with them since 1.1). They
  receive straight into the free capacity of your own `Vec<u8>` and may be mixed with
  `recv` on one stream, so a large message whose size is known, such as a body after its
  head, is written there by the kernel instead of being copied out of the runtime's 16 KiB
  pool buffers. The benchmark lab's own server (`bench/giong`) now completes uploads this
  way: on a 4-vCPU cloud VM, a 1 MiB upload costs 129 µs of server CPU instead of 234 and
  moves 8.0 GB/s instead of 4.4 (Tokio 8.0); 64 KiB uploads move 11% more.
