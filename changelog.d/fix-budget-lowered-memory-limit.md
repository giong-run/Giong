### Fixed

- **giong**: `Budget::set_limits` with a memory limit below a `Budget::reserve` that is
  waiting fails that reservation with `GIONG-E0081`, as it would have failed when asked.
  Before, it stayed first in the queue, could never be granted, and every reservation of
  the budget after it waited for ever (`try_reserve` refused them all)
  ([#412](https://github.com/giong-run/Giong/pull/412)).
