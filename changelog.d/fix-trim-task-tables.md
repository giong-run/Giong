### Fixed

- **giong**: a burst of tasks or timers no longer keeps its memory for good. A worker's task
  table, run queue and timer table only grew, so a service that once held a million tasks
  kept about 110 MB per worker after they had all finished; the worker now gives a mostly
  vacant table back before it sleeps, and hands out low slots first while something near the
  end of the table keeps it big. One million sleeping tasks on one worker: 120 MB left after
  `malloc_trim` before, 9 MB now (3 MB without timers). Spawning and timers cost no more
  instructions.
