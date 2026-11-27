### Added

- **all crates**: `SECURITY.md`: how to report a vulnerability (a private GitHub advisory
  with a reproducer), which versions get fixes (the latest 1.x, as a patch release) and the
  threat model: what giong defends against (memory safety, remote peers, the servers it
  connects to, ipc peers, files a project commits) and what is not a vulnerability (code in
  the same process, building an untrusted project, volume, local root).
