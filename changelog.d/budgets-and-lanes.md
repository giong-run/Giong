### Added

- **giong**: blocking and compute lanes (`giong::lane`): `spawn_blocking(f)` runs a closure that
  waits (`std::fs`, C libraries, `getaddrinfo`) on a pool of up to 128 threads
  (`Builder::max_blocking_threads`); `spawn_compute(f)` runs CPU-bound work on at most one
  thread per CPU of the process's budget (`Builder::compute_threads`), at nice +10 (the utility QoS class on macOS) so the
  workers get their core back as soon as they have work; `try_spawn_compute(f)` refuses
  with the new code `GIONG-E0014` (`Code::ComputeBusy`) once `Builder::compute_backlog`
  closures wait. All three return a `Send` `BlockingHandle<T>` that resolves to
  `Result<T, JoinError>`; a panic stays in the handle. `Handle::spawn_blocking`,
  `Handle::spawn_compute` and `Handle::try_spawn_compute` work from any thread, and
  `Handle::lane_stats()` reports threads, busy threads and queued closures. With one
  CPU-bound job per CPU running next to an echo server, the 99th percentile echo time
  stays at the idle level with the compute lane and grows several times without its
  lower priority or with Tokio's `spawn_blocking` (`bench/lanes.sh`, numbers in
  `bench/results/lanes/`). The epoll driver's file pool now shares the pool code
  (`src/pool.rs`) but stays a separate pool, so application jobs cannot starve file
  operations.

- **giong**: budgets per tenant (`giong::budget`): a `Budget` gives one tenant (a customer, an API
  key, a class of traffic) limits of its own (`Limits::tasks`, `memory`, `io`, `compute`)
  and charges each resource to the tenant while it holds it. `Budget::spawn`,
  `Budget::reserve` and `Budget::spawn_compute` wait for room, in order; `try_spawn`,
  `try_reserve` and `try_spawn_compute` refuse with the new code `GIONG-E0080`
  (`Code::BudgetFull`), and a reservation larger than the whole budget fails with
  `GIONG-E0081` (`Code::BudgetTooSmall`). `giong::spawn` and `giong::spawn_compute` inside a
  task of a budget charge the same budget; `Budget::enter(fut)` runs a future in a budget
  without a task of its own; `budget::current()` and `Budget::stats()` tell which budget
  runs and what it holds; `Budget::set_limits` changes the limits at run time. An I/O
  operation started in a budget holds one of its I/O slots until the kernel completes it:
  when its future is dropped, the slot stays with the operation's buffers until the
  cancellation completes. The compute lane keeps one queue per budget and serves the one
  that has used the least compute time for its `Limits::compute_weight`, so a tenant with
  long jobs cannot make another tenant's short jobs wait behind them. Code that uses no
  budget pays one test per operation start: 0 to 0.3% more instructions per operation
  (`scripts/instr.py`), and a spawn takes 1.5-1.9% fewer than before. In a container of 1 CPU
  and 128 MiB with tenant A sending far more than the box can serve, tenant B keeps its
  in-deadline successes with budgets, and loses them all without (`bench/tenants.sh`,
  numbers in `bench/results/tenants/`).
