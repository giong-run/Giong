# `giong::ipc` against giong's `UnixStream`, between two processes

`bench/ipc-giong.sh 5` on a cloud VM: 4 vCPU Intel Xeon @ 2.10 GHz, Linux 6.18.44,
io_uring driver. Two processes, one giong worker each, pinned to CPUs 0 and 1. Rows are
in [`raw.csv`](raw.csv); below are medians of 5 rounds, with the range in brackets. CPU is
the user plus system time of both processes, per message.

| Mode | Spin before park | Transport | Messages/s | µs per message | CPU µs per message |
|---|---|---|---|---|---|
| ping | 0 | ipc | 509k | 1.96 [1.42 to 4.97] | 2.92 |
| ping | 0 | unix | 34k | 29.8 [26.8 to 30.9] | 26.3 |
| ping | 50 µs | ipc | 1.10M | **0.91** [0.82 to 0.98] | **1.79** |
| ping | 50 µs | unix | 153k | 6.53 [5.89 to 6.55] | 12.9 |
| stream | 0 | ipc | 18.5M [10.2M to 23.0M] | 0.054 | 0.107 |
| stream | 0 | unix | 17.9M [16.1M to 19.2M] | 0.056 | 0.107 |
| stream | 50 µs | ipc | **10.1M** [7.5M to 28.3M] | 0.099 | 0.192 |
| stream | 50 µs | unix | 17.3M [16.6M to 17.8M] | 0.058 | 0.113 |

*ping*: one 64-byte message in flight, answered by the other process; µs is the round
trip. *stream*: 64-byte messages one way, as fast as the receiver takes them. The Unix
socket gets 32 messages per write, so it is measured at its best.

## What it shows

- **Round trips are 7.2 times faster with spinning workers** (0.91 against 6.53 µs) and
  use 7.2 times less CPU (1.79 against 12.9 µs). The receiver is watched from the
  worker's spin loop, so no system call is made on either side.
- **Without spinning, round trips are 15 times faster** (1.96 against 29.8 µs). Before
  it waits in the kernel, `recv` polls the ring for about a microsecond, and the answer
  usually arrives within that time. When it does not, the cost is a sleep and an eventfd
  wake-up, the same as the socket's. The range (1.42 to 4.97 µs) shows how often that
  happens.
- **Streaming without spinning is even** (18.5M against 17.9M messages/s, the same CPU
  per message), against a socket that is already batched 32 messages per write.
- **Loss: streaming with spinning workers is 42% slower than the socket** (median 10.1M
  against 17.3M messages/s), and the spread is wide (7.5M to 28.3M). Both processes
  stay hot, and the line holding the write position moves between the two cores on
  almost every message. The socket moves 32 messages per system call and is steady.
  Batching on the receiving side (taking what is there before the producer is told
  anything) is the next thing to try. Until then, a one-way stream is best served by
  workers that do not spin.

On this VM a wake-up through the kernel costs about 12 µs each way, which is why every
sleeping round trip is slow. Bare metal will lower both columns. The numbers here
compare the two transports on the same machine only.
