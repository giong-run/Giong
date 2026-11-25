### Security

- **giong-cli**: `giong upgrade --self`, `giong clean` and `giong bench --against` no longer
  work in temporary folders whose names another user of the machine can guess
  (`/tmp/giong-upgrade-<pid>`, `/tmp/giong-clean-probe-<pid>`, `/tmp/giong-bench-<repo>-<commit>`).
  On a shared Linux machine that user could make the folder first, between giong removing an
  old one and making it again, and swap the checked download before it was installed, or add
  a build script that cargo then ran as you. The download and the compiler probe now use a new
  folder giong made with mode 0700, and the older commit of `--against` is checked out inside
  `giong-<uid>`, a folder only you can enter (a folder of that name made by anyone else, or a
  link, is refused).
