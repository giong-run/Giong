### Fixed

- **giong**: the compute lane no longer remembers every budget that ever ran a closure in it.
  Each budget's fair-queue entry stayed after its last closure, even once the budget was
  dropped, so a server that makes a budget per tenant (or per API key) and drops it when the
  tenant goes quiet grew by one entry per budget for as long as it ran. Idle entries of dropped
  budgets are now swept whenever the entries double, at O(1) per new budget; a budget that is
  still alive keeps its place in the queue as before.
