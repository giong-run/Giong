### Security

- **giong**: on Windows, a forged datagram can no longer stop the thread that hands out the
  connections or datagrams of an address several workers share. Before, any datagram that
  reached its wake socket ended the thread for good, so the shared port stopped taking
  connections; another local program could send one. The wake socket's bind is now exclusive,
  and only a real stop ends the thread
  ([#316](https://github.com/giong-run/Giong/issues/316)).
