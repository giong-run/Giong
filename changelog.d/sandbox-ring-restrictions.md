### Added

- **giong**: `Builder::sandbox` and the `giong::sandbox` module, a sandbox that keeps io_uring.
  Seccomp cannot see io_uring's operations, which is why container runtimes block io_uring
  outright; giong restricts io_uring itself instead. Each worker's ring is created disabled,
  limited to the operations giong submits (`IORING_REGISTER_RESTRICTIONS`), then enabled; on
  Linux 7.0 every ring a covered thread creates later is limited the same way (per-task
  restrictions, with `no_new_privs` set), and `FileAccess::ReadOnly` adds classic BPF filters
  that refuse opens for writing in the kernel. Operations by path (`giong::fs`) are allowed as
  far as `Sandbox::files(FileAccess)` says, `None` by default, and refused otherwise with
  `GIONG-E0051` before the kernel is asked; the network and operations on open descriptors are
  untouched. `Sandbox::require` fails start with `GIONG-E0050` where a layer is missing (it
  needs Linux 7.0 for now), `Sandbox::best_effort` applies what the kernel has and says what it
  could not, and `Sandbox::audit` refuses nothing and logs each operation it would refuse once,
  as a JSON line. `giong::sandbox::report()` tells what a worker got; `GIONG_SANDBOX=none|
  read-only|read-write` gives a best-effort sandbox to a runtime that has none. Off by default:
  without it no operation pays anything. Seccomp, Landlock and dropping privileges are the next
  layers.
