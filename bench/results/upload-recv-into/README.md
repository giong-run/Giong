# Large request bodies received straight into the request buffer

`giong-http` used to copy every request body out of the runtime's pooled
receive buffers into its read buffer. Bodies of 64 KiB and more are now
received straight into the read buffer (`Transport::recv_into`). On io_uring,
a stream that does this also stops taking every byte into the pool: the next
large body stays in the socket until its head has been parsed. The stream
goes back to taking everything into the pool after 8 small messages in a row.

While such a body arrives, what has arrived is charged to the worker's budget
for requests being received after every receive, as before, so stalled uploads
are refused with `503` once the budget is full. The buffer grows by as much as
it holds, at least 1 MiB and at most 8 MiB at a time: a client that announces a
large body and sends only its head is given room for 1 MiB, not for what it
announced. After the request, a buffer that large is freed, not kept in the
worker's pool.

## Method

Cloud VM: 4 vCPU (Xeon 2.1 GHz), Linux 6.18. `bench-giong-http` runs on CPU 3,
and `bench/lab/loadgen` on CPUs 0 to 2 (3 threads, closed loop, 5 s after 1 s
of warmup). Uploads use 16 connections (`--mix /upload@1 --body N`); `/json`
uses 64. The mixed case is 16 connections where each request is a 1 MiB upload
with probability 1/20 and a `GET /json` otherwise
(`--mix /upload@1,/json@19 --body 1048576`), so connections that have just
received a large body go on to carry small requests. `main` is 5a4cdb3.
Versions run in turns; numbers are medians. CPU is the server's user and
system time per request, from `/proc`. Raw lines are in `runs.txt`.

The 1 MiB, 3 MiB and `/json` rows are from the final code. The 256 KiB, 64 KiB
and mixed rows are from the code just before the last change (the first
receive step of at most 1 MiB), which changes nothing for bodies of 1 MiB or
less: they get one step either way.

## Results

| driver | request | main req/s | branch req/s | ratio | main CPU µs | branch CPU µs |
|---|---|---:|---:|---:|---:|---:|
| io_uring | upload 3 MiB | 603 | 1,446 | 2.40× | 1,485 | 670 |
| io_uring | upload 1 MiB | 2,861 | 6,679 | 2.33× | 346 | 147 |
| io_uring | upload 256 KiB | 12,187 | 21,202 | 1.74× | 81 | 47 |
| io_uring | upload 64 KiB | 39,126 | 48,305 | 1.23× | 25.3 | 20.6 |
| io_uring | mixed (1 MiB uploads and `/json`) | 37,341 | 51,909 | 1.39× | 26.2 | 19.0 |
| io_uring | GET /json (8 rounds) | 136,865 | 136,240 | 1.00× | 7.24 | 7.26 |
| epoll | upload 3 MiB | 479 | 2,498 | 5.2× | 2,075 | 395 |
| epoll | upload 1 MiB | 3,542 | 7,667 | 2.16× | 279 | 130 |
| epoll | upload 256 KiB | 16,954 | 24,639 | 1.45× | 58 | 40 |
| epoll | upload 64 KiB | 40,311 | 47,525 | 1.18× | 24.6 | 20.8 |
| epoll | mixed (1 MiB uploads and `/json`) | 43,149 | 51,620 | 1.20× | 23.0 | 19.2 |
| epoll | GET /json (8 rounds) | 106,883 | 105,813 | 0.99× | 9.29 | 9.38 |

`/json` in the mixed case, on connections that also carry uploads:

| driver | main req/s | branch req/s | main p50 / p99 µs | branch p50 / p99 µs |
|---|---:|---:|---:|---:|
| io_uring | 35,465 | 49,302 | 329 / 1,315 | 233 / 903 |
| epoll | 40,982 | 49,029 | 294 / 1,118 | 249 / 817 |

`/json` alone is within noise: single runs range from 123,878 to 149,375 req/s
on io_uring and from 90,048 to 110,002 on epoll for both versions. Counted
instructions (`scripts/instr.py`) are +0.16% per `http1` request on io_uring
and +0.21% on epoll, within the gate.

The first A/B, before the review changes (4 KiB uploads, which never reach the
new code, were 0.99× on both drivers), is at the top of `runs.txt`.

For reference, axum on Tokio (`bench/axum`, same machine and method, 2
rounds): 1 MiB 3,947 to 3,986 req/s, 256 KiB 17,102 to 19,530, 64 KiB 43,144
to 43,826.

## Memory

Memory per idle connection (`scripts/instr.py`, `idle-http1`) is unchanged:
1,980 bytes on io_uring and 2,158 on epoll, the same as on main. Slow uploads
that announce 4 MB and send part of it hold the same RSS as on main (17.2 MB
against 17.1 MB for the whole server).
