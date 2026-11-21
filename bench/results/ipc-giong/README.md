# `giong::ipc` against giong's `UnixStream`, between two processes

`bench/ipc-giong.sh 5` on a cloud VM: 4 vCPU Intel Xeon @ 2.10 GHz, Linux 6.18.44,
io_uring driver. Two processes, one giong worker each, pinned to CPUs 0 and 1. Rows are
in [`raw.csv`](raw.csv); below are medians of 5 rounds, with the range in brackets. CPU is
the user plus system time of both processes, per message.

| Mode | Spin before park | Transport | Messages/s | µs per message | CPU µs per message |
|---|---|---|---|---|---|
| ping | 0 | ipc | 294k | **3.40** [2.19 to 5.70] | 4.54 |
| ping | 0 | unix | 35k | 28.3 [27.1 to 30.2] | 25.2 |
| ping | 50 µs | ipc | 1.00M | **1.00** [0.82 to 1.02] | 1.94 |
| ping | 50 µs | unix | 149k | 6.73 [6.51 to 7.14] | 13.3 |
| stream | 0 | ipc | **36.9M** [30.3M to 48.0M] | 0.027 | 0.054 |
| stream | 0 | unix | 17.6M [17.1M to 20.1M] | 0.057 | 0.112 |
| stream | 50 µs | ipc | **35.3M** [32.8M to 49.0M] | 0.028 | 0.057 |
| stream | 50 µs | unix | 17.9M [17.0M to 20.0M] | 0.056 | 0.107 |

*ping*: one 64-byte message in flight, answered by the other process; µs is the round
trip. *stream*: 64-byte messages one way, as fast as the receiver takes them. The Unix
socket gets 32 messages per write, so it is measured at its best.

## What it shows

`giong::ipc` is ahead in every row.

- **Round trips with spinning workers: 6.7 times faster** (1.00 against 6.73 µs), at 6.9
  times less CPU (1.94 against 13.3 µs). The receiver is watched from the worker's spin
  loop, so neither side makes a system call.
- **Round trips without spinning: 8.3 times faster** (3.40 against 28.3 µs). Before it
  waits in the kernel, `recv` polls the ring for about a microsecond, and the answer
  usually arrives within that time. When it does not, the cost is a sleep and an eventfd
  wake-up, the same as the socket's. The wide range (2.19 to 5.70 µs) shows how often
  that happens.
- **Streams: 2.0 to 2.1 times the socket's rate** (36.9M and 35.3M against 17.6M and
  17.9M messages/s) at half the CPU per message, against a socket that already gets 32
  messages per write. The sender publishes its write position once per turn of the
  worker (or once a sixteenth of the ring is waiting) rather than after every message.
  Without that, a spinning receiver took the position's cache line from the sender on
  nearly every message, and the spinning stream was 42% *behind* the socket (10.1M
  against 17.3M, first run of this benchmark).

On this VM a wake-up through the kernel costs about 12 µs each way, which is why every
sleeping round trip is slow. Bare metal will lower both columns. The numbers here
compare the two transports on the same machine only.
