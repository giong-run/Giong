# `giong::ipc` against giong's `UnixStream`, between two processes

`bench/ipc-giong.sh 5`, run twice, on a cloud VM: 4 vCPU Intel Xeon @ 2.30 GHz, Linux
6.18.44, io_uring driver. Two processes, one giong worker each, pinned to CPUs 0 and 1.
Rows are in [`raw.csv`](raw.csv); below are medians of 10 rounds, with the range in
brackets. CPU is the user plus system time of both processes, per message.

`ipc` receives with `Receiver::recv`, which copies each message out of the shared memory
(the safe default: a peer cannot change the bytes while they are read). `ipc in place`
receives with `Receiver::recv_in_place`, which reads them where they lie, for a peer the
caller trusts.

| Mode | Spin before park | Transport | Messages/s | µs per message | CPU µs per message |
|---|---|---|---|---|---|
| ping | 0 | ipc | 265k | **3.77** [2.20 to 4.68] | 4.84 |
| ping | 0 | ipc in place | 279k | **3.60** [2.04 to 4.81] | 4.63 |
| ping | 0 | unix | 34k | 29.3 [28.3 to 30.5] | 26.1 |
| ping | 50 µs | ipc | 1.11M | **0.90** [0.73 to 1.04] | 1.80 |
| ping | 50 µs | ipc in place | 1.15M | **0.87** [0.53 to 0.98] | 1.74 |
| ping | 50 µs | unix | 174k | 5.75 [5.42 to 6.40] | 11.4 |
| stream | 0 | ipc | **27.6M** [23.1M to 44.7M] | 0.036 | 0.072 |
| stream | 0 | ipc in place | **28.7M** [26.1M to 31.9M] | 0.035 | 0.070 |
| stream | 0 | unix | 19.8M [18.1M to 21.8M] | 0.051 | 0.099 |
| stream | 50 µs | ipc | **28.8M** [21.1M to 36.4M] | 0.035 | 0.069 |
| stream | 50 µs | ipc in place | **28.0M** [20.8M to 29.8M] | 0.036 | 0.071 |
| stream | 50 µs | unix | 20.7M [18.6M to 23.0M] | 0.049 | 0.097 |

*ping*: one 64-byte message in flight, answered by the other process; µs is the round
trip. *stream*: 64-byte messages one way, as fast as the receiver takes them. The Unix
socket gets 32 messages per write, so it is measured at its best.

## What it shows

`giong::ipc` is ahead of the socket in every row, with or without the copy.

- **Round trips with spinning workers: 6.4 times faster** (0.90 against 5.75 µs), at 6.3
  times less CPU (1.80 against 11.4 µs). The receiver is watched from the worker's spin
  loop, so neither side makes a system call.
- **Round trips without spinning: 7.8 times faster** (3.77 against 29.3 µs). Before it
  waits in the kernel, `recv` polls the ring for about a microsecond, and the answer
  usually arrives within that time. When it does not, the cost is a sleep and an eventfd
  wake-up, the same as the socket's. The wide range (2.20 to 4.68 µs) shows how often
  that happens.
- **Streams: 1.4 times the socket's rate** (27.6M and 28.8M against 19.8M and 20.7M
  messages/s) at about 0.7 times the CPU per message, against a socket that already gets
  32 messages per write. The sender publishes its write position once per turn of the
  worker (or once a sixteenth of the ring is waiting) rather than after every message.
  Without that, a spinning receiver took the position's cache line from the sender on
  nearly every message, and the spinning stream was 42% *behind* the socket (10.1M
  against 17.3M, first run of this benchmark).
- **The copy costs 3 to 5% at 64 bytes** in three rows (ping 3.77 against 3.60 µs and
  0.90 against 0.87 µs, stream without spinning 27.6M against 28.7M), within the
  run-to-run range, and the spinning stream is 3% faster with it (28.8M against 28.0M):
  `recv` gives the record back to the sender as soon as it is copied. For large messages
  the copy costs more: with 16 KiB messages that the receiver never reads, a stream ran
  at 0.8M to 1.0M messages/s copied against 2.6M to 2.7M in place (three runs each, same
  VM). That is what `recv_in_place` is for, between processes that trust each other.

An earlier run on a 2.10 GHz VM of the same kind, before `recv` copied, measured 3.40
and 1.00 µs for the two pings and 36.9M and 35.3M messages/s for the streams; the VMs
differ, so only rows from the same run are compared. On these VMs a wake-up through the
kernel costs about 12 µs each way, which is why every sleeping round trip is slow. Bare
metal will lower both columns.
