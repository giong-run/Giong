### Added

- **giong**: `GIONG-E0020` (`Code::ThreadLimit`): when the kernel refuses a worker thread
  with `EAGAIN`, `Runtime::run` says which one could not be started and why (a container's
  PID limit or `RLIMIT_NPROC`), with the container's `pids.max`, instead of a bare "Resource
  temporarily unavailable". Other errors (`ENOMEM`, say) are returned as they are.

### Fixed

- **giong**: workers that bind the same TCP or UDP port start under gVisor (GKE Sandbox,
  sandboxes that run code for AI agents). gVisor records `SO_REUSEPORT` when a socket binds,
  not when the next one does as Linux does, so every bind after the first failed with
  `AddrInUse` and a server with several workers did not start. giong now finds out once
  per process which way the system works, and on gVisor binds the socket that stays with
  `SO_REUSEPORT` from the start, after checking the port with a socket bound without it.
  Nothing changes on Linux.
- **giong**: under gVisor with its optional io_uring (`runsc --iouring`), which refuses
  giong's rings with `EINVAL`, falling back to epoll gives `GIONG-E0001` as the reason
  instead of a bare "Invalid argument".
- **giong**: the `GIONG-E0010` message names `DriverKind::IoUring` instead of
  `crate::DriverKind::IoUring`.
