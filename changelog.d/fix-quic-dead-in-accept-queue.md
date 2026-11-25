### Fixed

- **giong-quic**: a connection that dies while it waits to be accepted (idle timeout, the
  peer closed or reset it) leaves the accept queue and frees its place in the bound of
  1,024 connections not yet accepted. Before, an application that accepted slowly could
  find its endpoint refusing every new client because the queue was full of connections
  that no longer existed, and `accept` then handed out those dead connections first
  ([#222](https://github.com/giong-run/Giong/issues/222)).
