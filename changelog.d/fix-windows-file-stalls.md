### Fixed

- **giong**: on Windows, a worker no longer stops for as long as the disk takes when a task
  closes a file it wrote to or writes past the end of a file. `CloseHandle` on a written file
  runs the file system's cleanup and the filter drivers' scans, and NTFS runs a write past
  the data already written synchronously even on an overlapped handle; both ran on the
  worker, and in CI either took up to 0.85 s, during which the worker served nothing else
  (a cross-worker job waited 5 s, [#483](https://github.com/giong-run/Giong/issues/483)).
  Both now run on a blocking thread. In a 60 s soak the longest worker stall went from
  295-1192 ms to 18-100 ms, and the soak moved 20% more datagrams. Writing a new file is
  slower: 64 MiB in 64 KiB writes took 60 ms instead of 22 ms on `windows-2022`. A write
  inside the file, and the close of a file opened only for reading, stay on the worker.
