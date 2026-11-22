### Added

- **giong**: `giong::ipc`, message channels between two processes on one Linux host, over
  shared memory ([#189](https://github.com/giong-run/Giong/pull/189)). `IpcListener::bind`
  and `ipc::connect` meet on a Unix socket path, hand over a sealed memfd and four
  eventfds, and give each side a `Sender` and a `Receiver`. Messages are bounded (a quarter
  of the capacity), sending waits while the channel is full, and the receiver reads in
  place. The listener runs handshakes as tasks of their own, up to 64 at once with 5 seconds each, so a process that connects and sends nothing holds up no other ([#246](https://github.com/giong-run/Giong/issues/246)). A process that dies is reported (`GIONG-E0094`), and so is one that corrupts the
  ring (`GIONG-E0091`); the receiving process checks every position and length it reads.
  On a spinning worker (`spin_before_park`), a waiting receiver is watched from the spin
  loop and neither side makes a system call. A sender publishes a burst of messages with
  one store, at the end of the worker's turn or before it waits for an answer. Measured
  between two pinned processes on a 4 vCPU VM against giong's `UnixStream`: round trips
  6.7 times faster with spinning workers (1.00 against 6.73 µs) and 8.3 times without,
  streams of 64-byte messages 2.0 to 2.1 times the rate at half the CPU
  (`bench/results/ipc-giong/`). Codes `GIONG-E0090` to `GIONG-E0095`.
