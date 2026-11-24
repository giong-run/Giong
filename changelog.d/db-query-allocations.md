### Changed

- **giong-mysql**, **giong-postgres**: fewer allocations per statement. A request
  waited for by one task keeps its waker without a vector; giong-mysql no longer copies
  the column definitions that every run of a prepared statement sends again when they
  match the last run's, and giong-postgres reads what it needs of the command tag
  instead of keeping a copy. Measured with `bench/instr` (a query with one parameter
  and one row against a fake server): giong-mysql 27 to 23 allocations and 14,129 to
  13,711 instructions per query (2.96% fewer) on io_uring, 14,744 to 14,326 on epoll;
  giong-postgres 16 to 14 allocations, instructions unchanged (10,249 to 10,241).
  Both counts include the fake server's own allocations.
