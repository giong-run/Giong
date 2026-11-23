### Fixed

- **giong**: sources registered with `observe::register` that were dropped no longer stay
  in the worker's list (holding their allocation) until a `snapshot()`: registering drops
  them whenever the list has doubled. Every database pool registers one
  ([#249](https://github.com/giong-run/Giong/issues/249)).
- **giong**: a worker thread that panics while it starts now fails `Runtime::run` at once,
  with that panic, before any worker runs; the runtime used to run on the other workers and
  raise the panic only when it ended ([#250](https://github.com/giong-run/Giong/issues/250)).
- **giong**: CPU pinning reads and sets the affinity with a mask sized to the machine, so
  it keeps a container's CPU set and no longer panics a worker on machines with more than
  1024 possible CPUs ([#251](https://github.com/giong-run/Giong/issues/251)).
- **giong**: a task whose future panics while it is dropped after finishing now reports
  `JoinError::Panic`, not `JoinError::Cancelled`
  ([#252](https://github.com/giong-run/Giong/issues/252)).
