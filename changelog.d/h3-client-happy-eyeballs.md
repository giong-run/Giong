### Fixed

- **giong-http**: the HTTP/3 client races a host's addresses as Happy Eyeballs does (RFC 8305):
  the next address starts 250 ms after the previous one, or as soon as it fails, and the first
  connection made wins. Before, each address got its share of the connect timeout in turn, so
  `https://localhost` to a server listening on 127.0.0.1 waited 5 s on systems that resolve
  `localhost` to ::1 first (Windows), and a host with one dead address of two waited half of
  `connect_timeout` on every new connection.
