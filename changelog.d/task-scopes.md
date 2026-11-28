### Added

- **giong**: `giong::task`, structured concurrency. `Scope` runs futures that may borrow
  local data as children of one call: none outlives `Scope::run`, and the first error
  cancels the rest. `Scope::with_deadline` and `Scope::with_timeout` give a piece of work a
  deadline that everything inside it obeys: on io_uring the operations that wait on a peer
  (connect, one-off reads, waiting for a writable or readable descriptor) carry it into the
  kernel as a linked timeout, so the kernel cancels them at the deadline even while the
  worker is busy and data that arrives later stays in the socket; on the other drivers the
  scope's timer ends them. `deadline()` and `remaining()` let libraries pass the time left
  on. `TaskGroup` owns a changing set of tasks, gives results as they finish and aborts the
  rest on drop; its tasks inherit the deadline and task-local values. `task_local!` declares
  values every call inside a piece of work can read. New codes `GIONG-E0024` (deadline
  passed), `GIONG-E0025` (scope cancelled), `GIONG-E0026` (a scope run twice at once);
  `JoinError::DeadlineExceeded`; `IoUringFeature::LinkedDeadlines`
  (`GIONG_IO_URING_OFF=linked-deadlines` turns it off). Code that uses none of it runs as
  before: at most +0.3% instructions on the io_uring cases of `scripts/instr.py`, idle
  memory unchanged. `giong::spawn` and `giong::time::timeout` are unchanged.
