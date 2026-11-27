### Added

- **giong**: `fs::Root` opens paths beneath a directory and never outside it, for paths that
  come from a request, an upload or a configuration file: `Root::new(dir)`, then `open`,
  `create`, `read`, `metadata`, `open_root` (a root beneath it), or
  `OpenOptions::open_in(&root, path)`. A path that is absolute, has a `..` component or goes
  through a symbolic link (wherever it points) is refused with `GIONG-E0205`
  (`Code::OutsideRoot`), and nothing is created or emptied. On Linux the kernel refuses it
  during the open, so no check made beforehand can be raced: `openat2` with
  `RESOLVE_BENEATH`, `RESOLVE_NO_SYMLINKS` and `RESOLVE_NO_MAGICLINKS`, as an io_uring
  operation on the io_uring driver; where a seccomp filter hides `openat2`, a walk with
  `O_NOFOLLOW` that refuses the same paths. macOS uses `O_NOFOLLOW_ANY`; Windows opens each
  component without following reparse points, refuses symbolic links and junctions, and
  checks where the file it opened is. Tokio and Node have nothing like it built in; Go
  1.24's `os.Root` is the same idea.
