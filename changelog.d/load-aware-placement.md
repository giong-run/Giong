### Changed

- **Balanced listeners place new connections by how busy each worker has been, not only by
  how many tasks it holds.** `TcpListener::bind_balanced` now first compares the CPU time
  each worker used in the last few milliseconds, less time spent spinning for work, plus the
  average cost of a task for each connection it took on since; only workers within 5% of a
  core of the least busy one are compared by counts of tasks, as before. When every worker
  is at least 80% busy, a worker with clearly less work waiting gets the connection;
  otherwise counts decide, but never in favour of a worker whose task polls take over 25%
  longer. A worker kept busy by one hot connection used to be preferred for new connections
  over a worker holding 300 idle ones (it had the fewest tasks); now they go to the worker
  with room. In `bench/placement.sh` (2 workers, 4 vCPUs), short connections paced next to a
  hot one went from 508 to 198 µs at p99 and the hot connection got 9% more done; with 6 hot
  and 18 light connections arriving in random order, every measure matches the old rule (80
  rounds). Short connections sent back to back next to a hot one see a p50 2 µs higher (73
  against 71 µs, 3%) for the same p99. Nothing is added to the workers' loops: the accepting
  listener reads each worker thread's CPU time from the kernel, less the time it spent
  spinning before a sleep (now counted), at most every 5 ms. No connection ever moves once
  it is served ([#180](https://github.com/giong-run/Giong/pull/180)).
