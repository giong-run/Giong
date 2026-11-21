# Large request bodies received straight into the request buffer

`giong-http` used to copy every request body out of the runtime's pooled
receive buffers into its read buffer. Bodies of 64 KiB and more are now
received straight into the read buffer (`Transport::recv_into`). On io_uring,
a stream that does this also stops taking every byte into the pool: the next
large body stays in the socket until its head has been parsed.

Cloud VM: 4 vCPU (Xeon 2.1 GHz), Linux 6.18. `bench-giong-http` runs on CPU 3,
and `bench/lab/loadgen` on CPUs 0 to 2 (3 threads, closed loop). Uploads use
16 connections and `/json` uses 64; each run lasts 5 s after 1 s of warmup.
`main` is 5a4cdb3. The numbers are medians of 3 rounds, run in turns. Raw
lines are in `runs.txt`; CPU is the server's user and system time per request,
from `/proc`.

| driver | request | main req/s | branch req/s | ratio | main CPU µs | branch CPU µs |
|---|---|---:|---:|---:|---:|---:|
| io_uring | upload 1 MiB | 3,020 | 7,415 | 2.46× | 326 | 133 |
| io_uring | upload 256 KiB | 12,174 | 20,700 | 1.70× | 81 | 48 |
| io_uring | upload 64 KiB | 41,112 | 49,014 | 1.19× | 24.1 | 20.0 |
| io_uring | upload 4 KiB | 111,599 | 110,399 | 0.99× | 8.9 | 9.0 |
| io_uring | GET /json | 137,272 | 136,431 | 0.99× | 7.2 | 7.2 |
| epoll | upload 1 MiB | 3,698 | 7,622 | 2.06× | 268 | 130 |
| epoll | upload 256 KiB | 17,062 | 26,197 | 1.54× | 58 | 38 |
| epoll | upload 64 KiB | 41,061 | 48,432 | 1.18× | 24.1 | 20.4 |
| epoll | upload 4 KiB | 81,861 | 81,182 | 0.99× | 12.1 | 12.2 |
| epoll | GET /json | 104,161 | 107,457 | 1.03× | 9.4 | 9.2 |

Requests that do not reach the new path (4 KiB uploads, `/json`) are within
noise.

For reference, axum on Tokio (`bench/axum`, same machine and method, 2
rounds): 1 MiB 3,947 to 3,986 req/s, 256 KiB 17,102 to 19,530, 64 KiB 43,144
to 43,826.

Memory per idle connection (`scripts/instr.py`, `idle-http1`) is unchanged:
1,981 bytes on io_uring and 2,158 on epoll, the same as on main.
