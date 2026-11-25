### Fixed

- **giong**: on Linux, `fs::read_dir` reads a directory on a network or FUSE filesystem (NFS,
  SMB, 9p, Ceph, FUSE, or any filesystem giong does not know as local) on the blocking lane,
  so a batch that takes a round trip, or hangs with its server, holds up one blocking thread
  instead of the worker and every task on it. So is the rest of a directory once one batch
  took a millisecond or more on the worker (a cold disk). Local directories are read as
  before: listing 10,000 entries on tmpfs takes the same time
  ([#413](https://github.com/giong-run/Giong/pull/413), [#259](https://github.com/giong-run/Giong/issues/259)).
