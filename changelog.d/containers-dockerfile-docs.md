### Added

- **rest-api example**: a `Dockerfile` (a 44 MB distroless image) and a `compose.yaml` that
  runs it with PostgreSQL and Redis, a CPU and a memory limit, and the lines that allow
  io_uring.
- **docs**: DEPLOYMENT.md says how giong sizes its workers to a CPU limit, with measurements
  of why a fraction is rounded down; what a container's thread and file limits do, and that
  containerd starts containers at 1,024 open files, which caps a server near 1,000
  connections unless the program raises its soft limit; and how `SIGTERM` reaches process 1.
  llms.txt and AGENTS.md carry the short version.
