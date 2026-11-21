### Added

- **giong**: on Linux a runtime raises the soft limit on open files (`RLIMIT_NOFILE`) to the
  hard one when it is built, once per process, as Go programs do. containerd, and so most
  Kubernetes nodes, starts containers at 1,024 soft and 524,288 hard, which stopped a
  server near 1,000 connections. `Builder::raise_open_files_limit(false)` keeps the soft
  limit.
- **rest-api example**: a `Dockerfile` (a 44 MB distroless image) and a `compose.yaml` that
  runs it with PostgreSQL and Redis, a CPU and a memory limit, and the lines that allow
  io_uring.
- **docs**: DEPLOYMENT.md says how giong sizes its workers to a CPU limit, with measurements
  of why a fraction is rounded down; what a container's thread and file limits do, and that
  containerd starts containers at 1,024 open files; and how `SIGTERM` reaches process 1.
  llms.txt and AGENTS.md carry the short version.
