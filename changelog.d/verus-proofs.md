### Fixed

- **giong**: `ipc` receivers report a wrap marker at the start of the ring as corrupt
  (`GIONG-E0091`) instead of skipping a whole ring; a hostile sender that kept moving its
  position could otherwise keep a receive, and its worker, looping. Found by the new Verus
  proof ([#PR](https://github.com/giong-run/Giong/pull/PR)).

### Added

- **giong**: a Verus proof that the `ipc` ring never reads or writes outside its mapping,
  never panics and never loops for ever, for every capacity and whatever the other process
  writes into the shared memory, with no bound (`docs/verus.md`). It covers the shipped
  code itself: CI checks that the proof's copy of `src/ipc/ring.rs` is token for token the
  source, and that each planted bug is rejected. Nothing changes for users: no new
  dependency, no new code in the crate ([#PR](https://github.com/giong-run/Giong/pull/PR)).
- **giong**: a Verus proof that a worker never forgets a task: in every execution where
  threads that can run do run, every wake-up of a live task is followed by a poll that
  sees it, for any number of tasks, waking threads and queued entries. It covers a model
  of `RunFlag`, the doorbell, the injector, the run queue and the worker's sleep; CI fails
  when a function the model describes changes until the model is checked against it
  (`docs/verus.md`) ([#PR](https://github.com/giong-run/Giong/pull/PR)).
