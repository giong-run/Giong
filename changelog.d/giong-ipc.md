### Added

- **giong**: `giong::ipc`, message channels between two processes on one Linux host, over
  shared memory ([#189](https://github.com/giong-run/Giong/pull/189)). `IpcListener::bind`
  and `ipc::connect` meet on a Unix socket path, hand over a sealed memfd and four
  eventfds, and give each side a `Sender` and a `Receiver`. Messages are bounded (a quarter
  of the capacity), sending waits while the channel is full, and `Receiver::recv` copies
  each message out of the shared memory into a buffer it keeps, so a peer cannot change
  the bytes while they are read; `Receiver::recv_in_place` (`unsafe`) reads them where they
  lie, for a peer the caller trusts ([#319](https://github.com/giong-run/Giong/issues/319)). The listener runs handshakes as tasks of their own, up to 64 at once with 5 seconds each, so a process that connects and sends nothing holds up no other ([#246](https://github.com/giong-run/Giong/issues/246)). Descriptors received in a handshake are close-on-exec, so none leaks into a program the service runs, and a handshake given up while its message arrives closes them ([#286](https://github.com/giong-run/Giong/issues/286)). A process that dies is reported (`GIONG-E0094`), and so is one that corrupts the
  ring (`GIONG-E0091`); the receiving process checks every position and length it reads.
  On a spinning worker (`spin_before_park`), a waiting receiver is watched from the spin
  loop and neither side makes a system call. A sender publishes a burst of messages with
  one store, at the end of the worker's turn or before it waits for an answer. Measured
  between two pinned processes on a 4 vCPU VM against giong's `UnixStream`: round trips
  6.4 times faster with spinning workers (0.90 against 5.75 µs) and 7.8 times without,
  streams of 64-byte messages 1.4 times the rate at 0.7 times the CPU, with `recv`'s copy
  (which costs 3 to 5% at 64 bytes against `recv_in_place`; `bench/results/ipc-giong/`). Codes `GIONG-E0090` to `GIONG-E0095`.
