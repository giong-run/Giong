### Added

- **giong**: UDP offloads behind the `unstable-udp-offload` feature: GSO and per-datagram
  options (source address, ECN) through `UdpSocket::send_segments_to`, `send_to_with` and
  `SendOptions`; GRO, ECN and the local address on receive through `set_gro`,
  `set_recv_ecn`, `set_recv_local_addr` and `Datagram::segments`, `ecn` and `local_ip`.
  Off by default, at no cost to the default build
  ([#172](https://github.com/giong-run/Giong/pull/172)).
- **giong-quic**: a new crate, QUIC (RFC 9000) on giong: per-worker endpoints whose
  connection IDs name their worker, so datagrams that reach the wrong worker are passed on;
  bidirectional and unidirectional streams, 0-RTT, Retry under load, stateless resets and
  address validation tokens shared by every worker of a configuration
  ([#172](https://github.com/giong-run/Giong/pull/172)).
- **giong-http**: HTTP/3 (RFC 9114) behind the `unstable-http3` feature: `Server::serve_h3`,
  and a client that follows `Alt-Svc`, sends 0-RTT only for requests that are safe to replay,
  and falls back to the next address when one does not answer
  ([#172](https://github.com/giong-run/Giong/pull/172)).
