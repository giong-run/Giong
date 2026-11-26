### Fixed

- **giong-cli**: `giong test` no longer waits for a process a test started and left running (a
  server it never stopped). That process kept the output pipes open, so a suite that passed in
  seconds was reported as timed out after 30 minutes. The run now ends with cargo and stops
  what the tests left in its process group, so it does not hold a port into the epoll run.
- **giong-cli**: test output that is not UTF-8 no longer stops `giong test` reading it. The
  test went on writing into a closed pipe and failed with `Broken pipe`. Such bytes are now
  shown replaced, as in `giong dev`. `giong check`, `giong dev` and `giong upgrade` read
  cargo's output the same way.
