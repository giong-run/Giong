### Changed

- **giong-http** (performance): a keep-alive connection's idle and request timeouts and its
  shutdown signal are one alarm in its worker's timer wheel, armed once for the connection and
  moved to each new deadline (a later deadline only changes the one its entry keeps), instead
  of a timer armed and cancelled and a shutdown waker registered and removed around every wait.
  An HTTP/1.1 request costs 6.3% fewer instructions (`bench/instr`'s `http1`: 6,971 instead of
  7,439 on io_uring; the server alone 5,123 instead of 5,566, 8.0% fewer), giong-web's router
  4.5%, the client over HTTP/1.1 2.4%, HTTPS 1.6%; no case of `bench/instr` went up. The
  connections of a worker share one copy of the server's settings instead of a copy each, so
  every idle connection takes 112 bytes less (on io_uring: HTTP/1.1 1,150 bytes, HTTP/2 2,398,
  HTTPS 5,029, HTTP/2 over TLS 6,277). The idle timeout counts from the worker's clock of the
  current turn of its event loop, so it can end as much earlier as the turn has run. Method and
  raw data: `bench/results/http-conn-alarm/`.
