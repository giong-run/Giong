### Fixed

- **giong**: on the epoll and kqueue drivers, a task waiting on a `UdpSocket` is woken when
  another task's receive takes several datagrams from the socket at once. It used to stay
  asleep with a datagram queued for it until the next datagram arrived or its timeout fired
  ([#332](https://github.com/giong-run/Giong/issues/332)).
