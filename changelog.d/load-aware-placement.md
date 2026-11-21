### Changed

- **Balanced listeners place new connections by how busy each worker has been, not only by
  how many tasks it holds.** `TcpListener::bind_balanced` now first compares the CPU time
  each worker used in the last few milliseconds, less time spent spinning for work, plus
  the average cost of a task for each connection it took on since; only workers within 5%
  of a core of the least busy one are compared by counts of tasks, as before, and when
  every worker is at least 80% busy, by the work each has waiting. A worker kept busy by
  one hot connection used to be preferred for new connections over a worker holding 300
  idle ones (it had the fewest tasks); now they go to the worker with room. In
  `bench/placement.sh` (2 workers, 4 vCPUs, medians of 10 rounds), short connections paced
  next to a hot one went from 503 to 170 µs at p99 and the hot connection got 9% more
  done; with every worker saturated, and with mixed arrivals, the results match the old
  rule within the spread of the rounds. Nothing is added to the workers' loops: the
  accepting listener reads each worker thread's CPU time from the kernel, less the time it
  spent spinning before a sleep (now counted), at most every 5 ms. No connection ever moves
  once it is served
  ([#180](https://github.com/giong-run/Giong/pull/180)).
