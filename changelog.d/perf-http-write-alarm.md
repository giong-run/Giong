### Changed

- **giong-http** (performance): an HTTP/1.1 connection's write timeout waits on the
  connection's alarm, the one its reads wait on, instead of arming a timer of its own for
  every write that has to wait (which every write does while a busy worker batches its sends)
  and cancelling it; and `Request::method` and `Request::target` check their bytes as ASCII
  instead of UTF-8. Under load on one worker (bench/giong-http's `/json`, 32 connections) the
  server spends 3.8% fewer instructions per request: 5,381 instead of 5,593, of which timers
  175 fewer and the two text checks 52 fewer. In `bench/instr`, giong-web's router takes 3.4%
  fewer instructions per request (9,346 instead of 9,676 on io_uring), and an idle HTTP/1.1
  connection 24 bytes less (1,080 bytes on io_uring). Method and raw data:
  `bench/results/http-write-alarm/`.
