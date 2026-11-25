# Benchmarks

Every number giong publishes, with its method, the machine and where the raw output is.
Medians of interleaved rounds, ranges in parentheses, losses next to wins. The
[README](../README.md#results-at-a-glance) has the short version; recorded output is in
[`bench/results/`](../bench/results/), and every script is in [`bench/`](../bench/).

## Method


Every server implements the same three routes (`bench/`): `/json` (a 27-byte body),
`/users` (8 KB of JSON built per request) and `/compute` (a recursive Fibonacci).
`giong`, Monoio, raw Tokio and the hand-written epoll server are deliberately the same
program, with the same minimal HTTP parser, so they compare runtimes; `giong-http` is the
library, with strict parsing, a `Date` header, timeouts and limits. Each server is pinned
to its own core and `wrk` runs on another. Servers are measured in interleaved rounds and
the tables show the median, with the range in parentheses. Machine, except for the
runs under [One worker to N](#one-worker-to-n): a 4-vCPU virtual machine (Xeon
2.1 GHz), Linux 6.18, Rust 1.97, Node 22, Bun 1.4, Tokio 1.54, Monoio 0.2.4, Glommio
0.9.0, wrk 4.1.

"Monoio (tuned)" is Monoio with `DEFER_TASKRUN` passed through `uring_builder`. Monoio's
default ring has no such flag and is about 20% slower on the small response; comparing
against the default would flatter giong.

## Throughput at 50 connections

| Server | `/json` | `/users` (8 KB) | `/compute` | Pipelined `/json` | Memory |
|---|---|---|---|---|---|
| Node.js (`http`) | 39.5k | 14.3k | 1.3k | 54k | 87.5 MB |
| Bun | 147.7k | 22.1k | 2.4k | 126k | 27.2 MB |
| Axum (Tokio + hyper) | 161.0k | 18.3k | 4.4k | 18k | 4.8 MB |
| Raw Tokio, own parser | 174.1k (168–179k) | 35.7k (35–38k) | 4.5k | 1.52M | 3.8 MB |
| Hand-written epoll | 170.9k (159–175k) | 37.1k (36–37k) | 4.5k | 1.61M | 2.8 MB |
| Glommio | 143.9k (133–149k) | 36.4k (34–37k) | 4.4k | 1.06M | 15.3 MB |
| Monoio (default) | 136.8k (134–143k) | 63.6k (57–65k) | 4.1k | 1.20M | 3.7 MB |
| Monoio (tuned) | 176.0k (167–182k) | 64.4k (61–65k) | 4.2k | 1.56M | 3.7 MB |
| **giong** | 173.5k (165–180k) | 63.5k (63–66k) | 4.4k | 1.54M | 5.6 MB |
| **giong-http** | 195.7k (193–213k) | 58.0k (57–58k) | 4.0k | 1.58M | 6.0 MB |

Requests per second; "Pipelined" sends 16 requests per write; "Memory" is resident memory
after the run.

- **giong, tuned Monoio, raw Tokio and epoll are within noise on `/json`.** At this rate
  `wrk` is the bottleneck: in the `syscalls.sh` run below giong's core was busy 78% of the
  time.
- **`giong-http` beats the hand-written server on `/json`, and we do not know why.** It is
  not cheaper: per request it costs 7% more CPU (4.83 against 4.51 µs in the next table),
  as checking every header and writing `Date` should. It keeps its core busier (95%), so
  it gets more out of the same load generator. Treat this row as a measurement of the
  setup, not as a reason to prefer one over the other.
- **`/users` separates the batching runtimes from the rest**: giong and Monoio at 63–64k,
  everything else at 35–37k. They send a whole batch of responses in one system call; a
  server that issues one `send` per response wakes the other vCPU each time, which is
  expensive in a virtual machine. On physical hardware expect a smaller gap. `giong-http`
  is 9% behind giong here.
- **`/compute` is the same for everyone**, as it should be: the time goes into the
  function, not the runtime. Differences of a few percent here are code-layout noise.

## From one connection to thousands

`bench/scale.sh`, `/json`. "CPU" is the server's user plus kernel time per request, read
from `/proc`. It is the number to trust when the load generator is the limit.

| Connections | | giong | giong + spin | giong-http | Monoio (tuned) | Raw Tokio | Hand-written epoll | Glommio |
|---|---|---|---|---|---|---|---|---|
| 1 | requests/s | 32.4k | **47.9k** | 30.8k | 32.3k | 31.0k | 31.9k | 27.4k |
| | median latency | 28 µs | **19 µs** | 30 µs | 28 µs | 29 µs | 29 µs | 33 µs |
| 50 | requests/s | 171.7k | **204.8k** | 198.2k | 176.1k | 169.6k | 157.2k | 137.4k |
| | CPU per request | 4.51 µs | 4.85 µs | 4.83 µs | 4.60 µs | 4.74 µs | 4.50 µs | 7.14 µs |
| 1,000 | requests/s | 176.4k | 174.6k | 178.4k | 175.3k | 165.6k | 174.3k | 173.4k |
| | CPU per request | 4.39 µs | 5.29 µs | 4.85 µs | 4.48 µs | 4.74 µs | 4.40 µs | 5.66 µs |
| | p99 latency | 5.9 ms | **5.8 ms** | 7.2 ms | 8.1 ms | 50.0 ms | 7.3 ms | 10.1 ms |
| 5,000 | requests/s | **157.9k** | 147.7k | 153.0k | 141.3k | 124.3k | 138.3k | 117.6k |
| | CPU per request | **4.42 µs** | 5.12 µs | 5.37 µs | 5.25 µs | 5.74 µs | 4.99 µs | 8.29 µs |
| | p99 latency | **26 ms** | 33 ms | 34 ms | 44 ms | 54 ms | 32 ms | 71 ms |

- **Spinning is the clearest win.** With one connection every request finds a sleeping
  server; keeping it awake cuts median latency from 28 to 19 µs and raises throughput by
  48%. It costs CPU: 20.5 instead of 13.6 µs per request, and a spinning worker keeps its
  core busy between requests.
- **At 5,000 connections giong is the cheapest per request**, 11% below hand-written epoll
  and 16% below tuned Monoio, and has the lowest p99. The likely reason is the provided
  buffer ring: receive buffers are reused while still warm in cache, where the other
  servers keep a read buffer per connection. Throughput ranges at 5,000 connections are
  wide (giong 120–162k, tuned Monoio 103–156k), so the CPU column is the one to read.
- **At 50 and 1,000 connections giong, tuned Monoio and epoll are level** on CPU per
  request (4.4–4.6 µs); raw Tokio is 5–8% more expensive and Glommio 25–60%.
- p99 is the noisiest number in the table. Raw Tokio's 50 ms at 1,000 connections is the
  median of five rounds, not a single outlier, but it is still one machine.

## Memory per connection

`bench/memory.sh`: 10,000 connections from one client, four threads per server (giong
workers, Tokio worker threads, `GOMAXPROCS`), median of three rounds, a fresh server for
each measurement. The first three columns are the server's resident memory with the
connections held, minus before, per connection. "Idle" is a keep-alive connection that
has had one request answered, the usual state of a connection from a browser, an app or
a pool; "pending" has a request whose handler is waiting (`/wait`, as for a database or
another service). "Under load" is the peak while `wrk` keeps 5,000 connections busy.
giong's two rows were measured again after the packed receive buffers below (0.5), the
others are from 0.4's run on the same kind of VM.

| Server | Open | Idle | Pending | Under load |
|---|---|---|---|---|
| **giong-http** | **1.9 KiB** | **2.0 KiB** | **6.4 KiB** | 15.7 MiB |
| **giong-http** on epoll | 2.1 KiB | 2.1 KiB | **6.4 KiB** | **10.4 MiB** |
| Axum (Tokio + hyper) | 5.7 KiB | 20.4 KiB | 19.3 KiB | 115 MiB |
| Raw Tokio, own parser | 12.9 KiB | 13.1 KiB | 12.9 KiB | 64 MiB |
| Go `net/http` | 10.8 KiB | 18.9 KiB | 18.3 KiB | 155 MiB |

- **An idle giong connection holds no buffer.** What it costs is its task (about 1.2 KB
  for giong-http's connection), the multishot receive, an idle timer and the socket. The
  kernel picks a receive buffer only when data arrives; the request and response buffers
  are taken from a per-worker pool for the length of a request. Hyper keeps an 8 KiB read
  buffer per connection, Go a 4 KiB reader and writer and a goroutine stack.
- **A handler's future is on the heap only while it runs**, in blocks the worker reuses.
  In giong 0.3 it lived inside the connection's future, and every idle connection was as
  large as the largest future its handler could make: a handler holding 8 KiB across an
  `.await` cost 43 KiB per idle connection. Each wrapper that took a future by value
  (spawn, `timeout`, `until_shutdown`) also kept two copies of it; an idle giong-http
  connection took 10.9 KiB, one with a pending request 15.0 KiB.
- **"Pending" is mostly the request buffer**: 4 KiB, held while the handler runs, because
  the `Request` it was given points into it.
- **Receive buffers are a cost per worker, not per connection**, and only as many are
  touched as the traffic needs: a worker lends the kernel 16 at first and twice as many
  each time a turn of the event loop takes most of them, and reuses the most recently
  freed first. With 20 workers and 100 busy connections, giong-http's memory rises by
  1 MiB (it rose by 40 MiB when every worker lent all 512 buffers at once).
- **Small reads share a buffer.** On Linux 6.12 and newer the kernel fills a receive
  buffer piece by piece (`IOU_PBUF_RING_INC`), so a hundred connections that each send a
  short request use one or two buffers rather than a hundred. Before, each receive
  took a whole buffer, and under load a worker cycled through all it had lent: with 20
  workers and 5,000 busy connections giong-http peaked 50 MiB above idle, now 23 MiB
  (epoll 15 MiB, raw Tokio 64 MiB, Axum 124 MiB, Go 184 MiB; `WORKERS=20
  SCENARIOS=load`, in `bench/results/memory-20w/`). Requests per second did not change
  (eight interleaved rounds each at 4 and 20 workers, within this VM's noise).
- **Under load io_uring still holds more than epoll**: a response's buffer stays with the
  kernel until its send completes, and received data stays in its buffer until its
  connection runs, while epoll writes at once and reads only into a buffer it needs.
  Older kernels (and `GIONG_IO_URING_OFF=packed-buffers`) take whole buffers as before.
- Throughput under load is in the other tables; here the servers and `wrk` share four
  vCPUs.

## Where the time goes

`bench/syscalls.sh`, `/json` and `/users`, 50 connections, one run:

| | System calls per request | CPU per request, `/json` (user + kernel) | CPU per request, `/users` |
|---|---|---|---|
| Raw Tokio | 2.05 | 4.92 µs (0.84 + 4.08) | 27.9 µs |
| Hand-written epoll | 2.03 | 4.49 µs (0.39 + 4.10) | 27.1 µs |
| Glommio | 2.10 | 6.89 µs (1.33 + 5.55) | 29.2 µs |
| Monoio (default) | 0.04 | 7.21 µs (0.38 + 6.83) | 15.2 µs |
| Monoio (tuned) | 0.04 | 4.64 µs (0.57 + 4.06) | 16.7 µs |
| **giong** | 0.03 | 4.42 µs (0.65 + 3.77) | 16.0 µs |
| **giong-http** | 0.03 | 4.81 µs (1.22 + 3.59) | 17.4 µs |

The system-call count is the structural difference and the most robust result here. Its
effect on the small response is modest on this machine because a request costs about 4 µs
of kernel time whichever way it is delivered: on loopback the server's `send` also runs
the client's receive path. On the 8 KB response, where the batching runtimes send a whole
batch at once, it is worth almost half the CPU.

## io_uring against epoll

The same server program on both drivers (`GIONG_DRIVER`), against raw Tokio and the
hand-written epoll server from the [method](#method) above, on the same machine with the same
method, one core each. The first rows are `bench/run.sh` at 50 connections (medians of five
rounds), the next ones `bench/scale.sh` (three rounds) and `bench/syscalls.sh`:

| | giong, io_uring | giong, epoll | Raw Tokio | Hand-written epoll |
|---|---|---|---|---|
| `/json`: requests/s | 151.8k (141–167k) | 158.3k (156–166k) | 161.8k (158–168k) | 155.4k (148–164k) |
| `/users` (8 KB): requests/s | **55.7k** (53–59k) | 32.7k (31–35k) | 34.0k (29–36k) | 32.9k (31–35k) |
| Pipelined `/json`: requests/s | 1.43M | 1.44M | 1.43M | 1.45M |
| System calls per request | **0.04** | 2.02 | 2.05 | 2.03 |
| CPU per request, `/json` | 4.68 µs | 5.00 µs | 5.08 µs | 5.00 µs |
| CPU per request, `/users` | **18.7 µs** | 31.0 µs | 33.6 µs | 31.1 µs |
| 1 connection: median latency | 30 µs | 32 µs | 30 µs | 29 µs |
| 1,000 connections: p99 | 6.1 ms | 7.6 ms | 83.9 ms | 7.1 ms |
| 5,000 connections: requests/s | 150.0k | **152.6k** | 118.6k | 118.7k |
| 5,000 connections: CPU per request | **4.92 µs** | 5.72 µs | 6.09 µs | 5.51 µs |
| 5,000 connections: p99 | **37 ms** | 43 ms | 175 ms | 39 ms |

- **On epoll, giong is an epoll runtime like the others, and as fast as they are**: two
  system calls per request, and the same throughput as raw Tokio and the hand-written
  server on every route. At 5,000 connections it keeps the throughput of io_uring, 29%
  above the other two, at 16% more CPU per request than on io_uring.
- **What sets giong apart comes from io_uring**: a batch of responses in one system call,
  which on the 8 KB response is worth 70% more requests per second and 40% less CPU per
  request. Allow io_uring where you can.
- `giong-http` on epoll: 160.1k requests/s on `/json` against 175.6k on io_uring, and 1.05M
  against 1.28M pipelined; we have not found yet where the pipelined gap comes from.

## Two cores

The server gets two cores and starts one worker per core; `wrk` runs two threads on the
other two; 100 connections, three rounds.

| Server | `/json` | `/users` | `/compute` | Pipelined `/json` |
|---|---|---|---|---|
| Axum | 202.8k | 31.8k | 8.3k | 36k |
| Raw Tokio | 360.6k (354–361k) | 77.9k | 8.2k | 3.11M |
| Hand-written epoll | 305.5k | 83.1k | 8.4k | 2.66M |
| Glommio | 226.6k | 81.5k | 8.1k | 2.29M |
| Monoio (tuned) | 324.7k | 114.5k | 7.7k | 2.90M |
| **giong** | 304.7k (279–342k) | **118.4k** (104–122k) | 8.9k | 3.01M |
| **giong-http** | 350.4k (345–384k) | 107.9k | 8.9k | 3.06M |

**Not a ranking.** On four vCPUs the two `wrk` threads are as likely to be the limit as the
server: in an earlier two-core run giong's workers were only 73–94% busy. Raw Tokio and
`giong-http` lead on `/json`, giong and Monoio on `/users`, and the rest needs a bigger machine.

## One worker to N

`bench/workers.sh` measures how throughput grows with the number of workers: the server
gets the first N of its CPUs, the connection count grows with N (64 per worker), and the
report shows, besides requests per second, the CPU time per request and how busy the
server's and the load generator's CPUs were, so a run says which side set the number.
It can drive the server from a second machine over ssh (`LOAD_HOST`, `BENCH_HOST`), which
is how it should be run.

**On bare metal.** Two identical machines, AMD EPYC 4345P (8 cores, 16 threads, boost on),
64 GB, Intel E810 over a 25 GbE private network (23.5 Gb/s measured with `iperf3`): the
server on one, `wrk` with 16 threads on the other. Linux 7.0, giong on io_uring, code as
of 0.4.0; `/json`, five rounds of 6 s. Requests per second, median:

| Server | 1 worker | 2 | 4 | 8 | 16 ¹ |
|---|---|---|---|---|---|
| giong | **509k** | **928k** | **1.51M** | **2.45M** | 2.52M |
| giong-http | 437k | 792k | 1.32M | 2.35M | 2.49M |
| Monoio (tuned) | 489k | 893k | 1.46M | 2.44M | 2.53M |
| Raw Tokio | 508k | 868k | 1.36M | 2.16M ² | 2.33M |
| Glommio | 440k | 796k | 1.31M | 2.21M | 2.28M |

CPU time per request, in µs:

| Server | 1 | 2 | 4 | 8 | 16 |
|---|---|---|---|---|---|
| giong | 1.86 | 2.13 | 2.64 | 2.95 | 3.54 |
| giong-http | 2.27 | 2.51 | 3.02 | 3.33 | 3.79 |
| Monoio (tuned) | 2.00 | 2.24 | 2.73 | 3.03 | 3.63 |
| Raw Tokio | 1.92 | 2.17 | 2.65 | 2.77 | 3.44 |
| Glommio | 2.27 | 2.51 | 3.05 | 3.59 | 5.16 |

¹ 16 workers share the 8 cores (SMT siblings). Every server levels off there with neither
side saturated, and the median p99 goes above 100 ms for four of the five (raw Tokio:
1.7 ms).
² Neither side was saturated (server 78% busy, load generator 69%).

- **From 1 to 4 workers the servers were the limit** (their cores 92–97% busy, the load
  generator 13–45%), so these rows measure the servers. giong had the highest median at
  every worker count up to 8, level with raw Tokio at 1 worker (509k against 508k) and
  with tuned Monoio at 8 (2.45M against 2.44M).
- **From 1 to 8 workers giong grew 4.8 times**, Monoio 5.0, Glommio 5.0 and raw Tokio
  4.3. The CPU time per request rises with the number of workers for every server alike
  (giong 1.86 to 2.95 µs); boost, which lowers the clock as more cores get busy, is
  probably part of it.
- **On Ubuntu's 6.8 kernel** (`linux-6.8-epoll/`), giong fell back to its epoll driver
  (the kernel bug that [#52](https://github.com/giong-run/Giong/pull/52) works around), and
  every server, Tokio included, was faster from 4 workers on than under 7.0, which points
  at the kernel or the network driver: compare runtimes within one run, not across runs.
  There, on epoll, giong was 4–6% behind tuned Monoio from 1 to 8 workers and ahead of raw
  Tokio from 2 workers on (2.84M against 2.64M at 8).

Recorded output, with the machines, the method and what limited each row:
`bench/results/bare-metal-epyc-4345p/`.

The 0.5.0 release candidate (d5a0b0c) on the same machines, with 16 queues on the card,
three rounds; requests per second, median:

| Server | 1 worker | 2 | 4 | 8 ³ | 16 ³ |
|---|---|---|---|---|---|
| giong | **560k** | **1.06M** | **1.75M** | **2.94M** | 3.24M |
| giong-http | 458k | 858k | 1.49M | 2.60M | 3.12M |
| Monoio (tuned) | 524k | 997k | 1.69M | 2.87M | 3.24M |
| Raw Tokio | 556k | 962k | 1.59M | 2.54M | 2.82M |

³ From 8 workers on, `wrk` on the second machine is the limit with 16 queues (95–99.5%
busy in earlier runs), which is why giong and Monoio tie at 16. From 1 to 4 workers the
servers were 91–97% busy. Recorded in
`bench/results/bare-metal-epyc-4345p/linux-7.0/release-0.5.0/`.

**On 14 cores.** An Intel i5-13500 desktop (6 performance cores and 8 efficiency cores,
20 threads, 64 GB) running Linux 6.18 under WSL2 on Windows 10. The server gets vCPUs 0–9,
`wrk` gets 10–19 with ten threads, and they talk over loopback; `/json`, three rounds of
6 s. Requests per second, median (the range for giong):

| Server | 1 worker | 2 | 4 | 6 | 8 | 10 |
|---|---|---|---|---|---|---|
| giong | 178k (165–181k) | 437k (414–462k) | 934k (818k–1.04M) | 1.24M (1.22–1.34M) | 1.50M (1.37–1.50M) ¹ | 1.33M ¹ |
| giong-http | 161k | 371k | 776k | 1.01M | 1.26M | 1.26M |
| Monoio (tuned) | 167k | 352k | 928k | 1.30M | 1.30M | 1.35M ¹ |
| Raw Tokio | 131k | 323k | 777k | 1.07M | 1.03M ² | 1.16M ² |
| Glommio | 172k | 322k | 768k | 1.06M | 984k | 1.13M |

CPU time per request, in µs:

| Server | 1 | 2 | 4 | 6 | 8 | 10 |
|---|---|---|---|---|---|---|
| giong | 5.58 | 4.58 | 4.29 | 4.79 | 5.26 | 6.66 |
| giong-http | 6.21 | 5.42 | 5.17 | 5.95 | 6.26 | 7.60 |
| Monoio (tuned) | 5.95 | 5.70 | 4.33 | 4.55 | 5.91 | 6.64 |
| Raw Tokio | 7.65 | 6.19 | 5.09 | 5.41 | 6.66 | 6.60 |
| Glommio | 5.80 | 6.24 | 5.24 | 5.68 | 8.11 | 8.75 |

¹ `wrk` was the limit (its CPUs 94–97% busy), so the number is a lower bound.
² Neither side was saturated (server 76–86% busy).

- **From 1 to 6 workers every server was the limit** (its CPUs 95–100% busy), so these
  rows measure the servers. giong had the highest median at 1 worker (Monoio's best round
  was higher) and was clearly ahead at 2, level with Monoio at 4 (the ranges overlap) and
  behind it at 6 (1.24M against 1.30M; Monoio's range was 876k–1.33M).
- **giong used the least CPU per request from 1 to 4 workers** (tied with Monoio at 4):
  4.3–5.6 µs, against 5.1–7.7 µs for raw Tokio.
- **The highest number of the run, 1.50M requests/s, was giong at 8 workers**, with `wrk`
  as the limit.
- **At 10 workers the machine is overloaded**: server and `wrk` keep 20 vCPUs busy on 14
  physical cores, everything flattens or drops, and p99 goes above 2 ms.

This run says nothing reliable about scaling efficiency. Going from one worker to two
gives 2.1 to 2.5 times the throughput for four of the five servers, which means the
one-worker numbers are too low, not that two workers do more than twice the work: WSL2
does not pin vCPUs to physical cores, and Windows 10 may run a lone busy vCPU on an
efficiency core. Some rows also vary by up to a third between rounds; read the bare-metal
run above for scaling. Recorded output, with the machine details and how the run was
checked: `bench/results/workers-i5-13500-wsl/`.

**On 4 vCPUs** (the virtual machine used for the other tables), two `wrk` threads on the
other two vCPUs could no longer keep up once the server had two workers, so that run only
shows the one-worker numbers (giong 218.9k, Monoio 214.2k, giong-http 186.1k, Glommio
175.8k, raw Tokio 151.1k). Recorded output: `bench/results/workers/`.

## HTTPS

`bench/tls.sh`: `giong-http` + `giong-tls` against Axum + `axum-server`, both on rustls with
`ring`, so the TLS library and the cryptography are the same and only the runtime, the
HTTP layer and the TLS settings differ. The server gets one core and `wrk` three (the load
generator has to encrypt too); 50 connections, TLS 1.3, an ECDSA P-256 certificate, seven
interleaved rounds on a 4-vCPU VM (2026-11-17, [`bench/results/tls/`](../bench/results/tls/README.md)).
The server's core was 97–100% busy in every row, so the server was the limit. The plain
HTTP rows are from the earlier session of five rounds (`bench/results/tls/2026-11-12/`).

| Server | `/json` | `/users` (8 KB) | New connection per request | CPU per new connection |
|---|---|---|---|---|
| Axum, plain HTTP | 94.0k (86–97k) | 19.5k | | |
| giong-http, plain HTTP | 150.3k (140–158k) | 55.3k (54–59k) | | |
| Axum + rustls | 81.2k (80–85k) | 18.5k (17–19k) | 7.1k (6.3–7.6k) | 136 µs |
| Axum + rustls, set to pick AES-128-GCM first | 80.5k (78–83k) | 19.4k (19–20k) | 8.4k (7.7–8.6k) | 116 µs |
| giong-http + giong-tls 0.8.0 | 145.1k (129–150k) | 47.7k (46–50k) | 6.8k (6.3–7.1k) | 142 µs |
| **giong-http + giong-tls** | **146.6k (136–155k)** | **48.7k (47–50k)** | **8.4k (7.9–9.1k)** | **116 µs** |

- **On kept-alive connections giong's HTTPS is 1.8 times Axum + rustls on `/json` and 2.6
  times on `/users`.** Most of the `/users` factor is there without TLS (see above).
- **TLS costs giong 2% on `/json` and 12% on `/users`**, where there are more bytes to
  encrypt.
- **New connections: 18% more per second than Axum + rustls, at 15% less CPU each, and
  lower latency (p99 4.2 ms against 7.1).** wrk resumes sessions, so these are resumed TLS
  1.3 handshakes. Two changes since 0.8.0 (which was 5% behind Axum here): giong-tls sends
  its session tickets with the first response and its `close_notify` with the FIN (14.4
  TCP segments per connection, both directions, against 16.5 before and Axum's 14.0), and
  `TlsAcceptor::with_certificates` picks AES-128-GCM first, as Go's and BoringSSL's servers do,
  where wrk (OpenSSL) lists AES-256-GCM first and its suite hashes with SHA-384. **The
  suite is most of the gain: Axum set up the same way matches giong's rate** (8.4k), with
  higher latency (p99 6.2 ms against 4.2). A handshake is almost all cryptography, and
  both use the same library for it.
- With 64 KiB responses giong-http + giong-tls serves 31.9k req/s against 26.3k for 0.8.0
  (+21%), from AES-128-GCM.
- Axum is lower here than in the first table (94k against 161k) because `wrk` runs three
  threads: requests arrive less bunched and Tokio gets less work out of each `epoll_wait`.
  giong hardly depends on the shape of the load.
- One connection per request is not measured for plain HTTP: on loopback that test runs
  out of client ports (TIME_WAIT) at about 28,000 per second and measures the client.

**Kernel TLS pays off for large files, not for data in memory.** On bare metal (EPYC
4345P, Linux 7.0, the server on one core), giong-http serving a 1 MiB file with
`Body::file` and kernel TLS reaches 6,104 req/s against 4,484 with rustls in the process
(+36%, and 164 µs of CPU per request against 223): `sendfile` lets the kernel encrypt
straight from the page cache. nginx with kernel TLS and `sendfile` does 6,039 on the
same machine. With 64 KiB files, with bodies from memory and with small responses the
kernel is 4–22% slower than rustls, so `Ktls::Off` stays the default. A client receiving
64 KiB bodies gains 11% from letting the kernel decrypt. The whole table and a profile
are in the [`giong-tls` README](../giong-tls/README.md#when-it-pays-off).

## PostgreSQL

`bench/postgres.sh`: [`giong-postgres`](../giong-postgres/README.md#performance) against
tokio-postgres with deadpool-postgres, each client on one core, the same PostgreSQL 16
server on the other three, 64 tasks issuing statements back to back. Medians, statements
per second:

| Client, connections | One row by key | p99 | 100 rows | p99 |
|---|---|---|---|---|
| **giong-postgres, 3** | **311.4k** | **0.43 ms** | **71.3k** | **2.0 ms** |
| tokio-postgres + deadpool, 4 | 53.1k | 1.9 ms | 21.7k | 4.4 ms |
| tokio-postgres + deadpool, 64 (its best) | 93.8k | 1.5 ms | 28.6k | 4.1 ms |

Statements from every task share the pool's connections and are pipelined, so one client
core does 3.3 and 2.5 times the work of deadpool at its best. Over one shared connection
both libraries pipeline and the server's single backend is the limit; there giong-postgres
is 7–9% behind tokio-postgres at the median (47.7k against 51.4k by key). The crate's README has the full table, including what happens with more
connections than the server has cores.

With `Config::batch_reads(true)` (new in 1.1, off by default), the `SELECT`s waiting for
one shared connection go to the server under one `Sync`, at most 16, so the server replies
and wakes the client once per batch. `bench/postgres.sh` in `batch` mode, 64 tasks, one row
by key (`bench/results/postgres/batch/`):

| Server cores | Connections | Without batches | p99 | With batches | p99 |
|---|---|---|---|---|---|
| 1 | 1 | 58.7k (54–61k) | 2.0 ms | **198.3k** (197–204k) | **0.56 ms** |
| 1 | 4 | 50.5k (49–55k) | 12.5 ms | **171.9k** (167–187k) | **0.75 ms** |
| 3 | 4 | 314.2k (273–327k) | 2.5 ms | **370.4k** (318–372k) | **0.39 ms** |

3.4 times the lookups where the database is the limit, 18% more with it on three cores, and
nothing for 100-row reads, where producing the rows costs far more than the reply. On the
full path of an HTTP request (`bench/db-path.sh`) it gains little, because there the HTTP
side is the limit. When to leave it off (row-level security set up per request) is in
the [giong-postgres README](../giong-postgres/README.md#batched-reads).

## MySQL

`bench/mysql.sh`: [`giong-mysql`](../giong-mysql/README.md#performance) against mysql_async and
SQLx on Tokio, each client on one core with its own pool, the same MySQL 8.0 server on the
other three, 64 tasks issuing statements back to back. Medians, statements per second:

| Client, connections | One row by key | p99 | 100 rows | p99 |
|---|---|---|---|---|
| **giong-mysql, 3** | **150.7k** | **0.81 ms** | **38.9k** | **3.4 ms** |
| SQLx, 64 (its best) | 29.3k | 3.9 ms | 10.4k | 9.1 ms |
| mysql_async, 4 (its best) | 19.0k | 5.5 ms | 8.9k | 10.6 ms |

Statements from every task share the pool's connections and are pipelined, so one client
core does 5.1 and 3.7 times the work of SQLx at its best, and the server becomes the limit.
One connection shared by every task does about what SQLx does with 64. The crate's README
has the full table, including mysql_async's default pool settings and more connections
than the server has cores.

## Redis

`bench/redis.sh`: [`giong-redis`](../giong-redis/README.md#performance) against redis-rs (a
`MultiplexedConnection`) and fred on Tokio, all three pipelining the commands of 64 tasks,
each client on one core, the same Redis 7.0 server on the other three. Medians, commands
per second, and the client core's time per command:

| Client, connections | `GET` 64 B | µs/op | `SET` 64 B | µs/op | `MGET` of 10 | µs/op |
|---|---|---|---|---|---|---|
| **giong-redis, 1** | **640k** | **0.80** | **565k** | **0.83** | **268k** | **1.75** |
| redis-rs, 1 | 492k | 1.59 | 493k | 1.52 | 166k | 4.16 |
| fred, 1 | 351k | 2.22 | 316k | 2.37 | 117k | 6.17 |
| **giong-redis, 4** | **848k** | **1.06** | **842k** | **1.03** | **375k** | **2.03** |
| redis-rs, 4 | 483k | 2.01 | 516k | 1.88 | 209k | 4.68 |

1.1 to 1.8 times redis-rs, at half its CPU time per command or less. With 8 KiB values,
where about half of every client's core is time in the kernel, giong-redis does 2.5 times
redis-rs on one connection and 1.25 times on four. The crate's README has the full tables
and how many connections to use.

## gRPC

`bench/grpc.sh`: [`giong-grpc`](../giong-grpc/README.md#measured-against-tonic) against
tonic 0.14 (prost, hyper, Tokio) with the same service. One server thread, loaded from three
other cores until its core is the limit, with tonic's client and giong-grpc's as the load
generator; a 4-core cloud VM, io_uring, the median of 10 rounds of 5 s
(`bench/results/grpc/`):

| Workload | Load generator | tonic | giong-grpc | | Server CPU per call or message |
|---|---|---|---|---|---|
| unary, 100 B | tonic | 172,842 calls/s | **259,030** | +50% | 5.54 → 3.62 µs |
| unary, 100 B | giong-grpc | 174,525 | **227,408** | +30% | 5.46 → 4.14 µs |
| unary, 10 kB | tonic | 71,918 | **89,862** | +25% | 13.25 → 10.44 µs |
| unary, 10 kB | giong-grpc | 75,010 | **90,463** | +21% | 12.77 → 10.51 µs |
| bidirectional round trips | tonic | 550,596 msg/s | **813,260** | +48% | 1.74 → 1.18 µs |
| server stream | tonic | 2,917,860 msg/s | **11,694,153** | 4.0× | 0.329 → 0.081 µs |

No server workload is behind. A one-thread giong-grpc client against a three-thread server
is level to 14% ahead of tonic's client, and level (−1.7%) on round trips against tonic's
server, where both clients used three quarters of their core and the row measures the
server. With 1,000 bidirectional streams open on 10 connections, the server's memory grew by
5.7 kB a stream with giong-grpc and 11.6 kB with tonic. Not measured: client streams of
many messages in a row.

## Routing, JSON and middleware

`bench/web.sh`: [`giong-web`](../giong-web/README.md#performance) against axum 0.8 with the
same serde handlers, each on one core, 50 connections. Medians of 5 rounds, requests per
second:

| Server | `/json` | `/users` (8 KB) | `/users/{id}` | With request ids, gzip, timeout: `/json` | `/users` |
|---|---|---|---|---|---|
| **giong-web** | **152.5k** | **28.0k** | **140.5k** | **126.2k** | **12.0k** |
| axum | 118.3k | 15.2k | 107.1k | 65.8k (tower-http) | 5.2k |

The router, parameters and serde cost giong-web about 5% against giong-http's bare handler
on `/json` in the same run. `/compute` is the same for both within noise; its p99 is higher
on giong-web (22.9 against 18.5 ms).

## Large uploads

Since 1.1, giong-http receives a request body of 64 KiB or more straight into the request
buffer instead of copying it out of the runtime's receive buffers
([#194](https://github.com/giong-run/Giong/pull/194)). `bench-giong-http` on one core of a
4-vCPU VM, `bench/lab/loadgen` on the other three, 16 connections, before (`main` at
5a4cdb3) and after, versions in turns, medians
([`bench/results/upload-recv-into/`](../bench/results/upload-recv-into/README.md)):

| Driver | Request | Before, req/s | After, req/s | | CPU per request |
|---|---|---:|---:|---:|---|
| io_uring | upload 3 MiB | 603 | **1,446** | 2.40× | 1,485 → 670 µs |
| io_uring | upload 1 MiB | 2,861 | **6,679** | 2.33× | 346 → 147 µs |
| io_uring | upload 64 KiB | 39,126 | **48,305** | 1.23× | 25.3 → 20.6 µs |
| io_uring | 1 MiB uploads mixed with `/json` | 37,341 | **51,909** | 1.39× | 26.2 → 19.0 µs |
| io_uring | `GET /json` | 136,865 | 136,240 | 1.00× | 7.24 → 7.26 µs |
| epoll | upload 3 MiB | 479 | **2,498** | 5.2× | 2,075 → 395 µs |
| epoll | upload 1 MiB | 3,542 | **7,667** | 2.16× | 279 → 130 µs |
| epoll | `GET /json` | 106,883 | 105,813 | 0.99× | 9.29 → 9.38 µs |

Small requests alone and the memory of an idle connection are unchanged. On the bare
runtime, without giong-http, Tokio still receives large uploads faster: see the
[benchmark lab](#benchmark-lab).

## File I/O

`bench/fs.sh`: 200,000 operations per measurement, five rounds, all data in the page cache,
so this measures what one operation costs, not the disk. giong, Monoio, compio and `std` use
one thread. Tokio hands every file operation to its blocking thread pool, so it is measured
pinned to one core and free to use all four. Raw data: [bench/results/fs](../bench/results/fs/summary.txt).

| Operations per second | Whole 4 KiB file, 1 task | 64 tasks | 4 KiB at a random offset, 1 task | 64 tasks |
|---|---|---|---|---|
| `std` (blocking) | 567k | | 1.31M | |
| **giong** | **437k** (395–445k) | **517k** | **1.33M** (1.28–1.35M) | **1.30M** |
| Monoio 0.2 | 375k | 338k | 968k | 1.23M |
| compio 0.19 | 141k | 130k | 892k | 1.10M |
| Tokio, one core | 135k | 125k | 163k | 157k |
| Tokio, 4 cores | 33k | 98k | 38k | 96k |

- **Reads at an offset: 8 times Tokio, level with a blocking `std` call.** giong first copies
  from the page cache with `preadv2(RWF_NOWAIT)`, one system call that never waits for the
  disk, and goes through the ring only when the data is not cached. On Tokio every read is a
  hand-off to another thread and back.
- **Whole small files (open, fstat, read, close): 3.2 to 4.1 times Tokio, 1.2 to 1.5 times
  Monoio.**
- **A blocking `std` call is still faster for whole small files** (567k against 437k): opening
  and closing go through the ring. What giong buys is a worker that never waits when the data
  is not cached, which this benchmark does not measure.

Measured on 2026-11-17 on the same machine; the run before the page-cache path is in
[bench/results/fs/before-core-pr](../bench/results/fs/before-core-pr/summary.txt).

## Core primitives

`bench/core.sh 5`: tasks, channels, timers and loopback TCP against Tokio (current thread,
`LocalSet`, multi-thread), Monoio, Glommio, compio and smol, each on its own scheduler and
channels, pinned to two cores. Median nanoseconds per operation over five rounds, lower is
better; giong on io_uring (epoll in parentheses). Method, every runtime and the raw data:
[bench/results/core/vm](../bench/results/core/vm/README.md).

| ns per operation | giong | Tokio (best) | Monoio | Glommio | compio | smol |
|---|---|---|---|---|---|---|
| spawn and join a task | **91.5** (93.3) | 174 | 59.1 | 91.5 | 89.9 | 272 |
| yield | **23.7** (28.7) | 62.3 | 49.5 | 45.0 | 30.5 | 138 |
| unbounded channel, one producer | **30.9** (24.3) | 57.9 | 13.8 | 23.4 | 118 | 109 |
| oneshot answered by a new task | **293** (276) | 527 | 218 | 208 | 415 | 1,793 |
| 1,000 tasks pass a token | **220** (224) | 289 | 91.8 | 343 | 585 | 484 |
| 100,000 sleeping tasks, per task | **688** (736) | 886 | 1,497 | 1,328 | 951 | 1,231 |
| a plain thread feeds a task | **64.0** (68.9) | 155 | 185 | 170 | 223 | 211 |
| one byte back and forth over TCP | **4,652** (5,304) | 5,231 | 5,181 | 13,517 | 5,257 | 8,851 |
| resolve `localhost` | **171** (186) | 28,564 | | | | 28,643 |

- **giong is ahead of every Tokio flavour in every row**, except TCP round trips on epoll,
  where both make the same system calls and are level within noise.
- **Monoio and Glommio are ahead on spawning, channels and oneshots.** Their tasks and channels
  only work within one thread, with no atomic operations; giong's carry values between threads
  and workers as well (the "plain thread" row, where giong is 2.4 to 3.5 times faster than every
  other runtime).
- `localhost` is answered from giong's cache of `/etc/hosts`; Tokio and smol call
  `getaddrinfo` on a blocking thread every time. Monoio, Glommio and compio have no resolver.

## UDP

`bench/udp-echo.sh`: a UDP echo server on one core, 64-byte datagrams, the client in
another network namespace over veth keeping 256 datagrams in flight, five rounds of 8 s
(`bench/results/udp-echo/`). "Kernel × 32" is a server with no runtime that calls
`recvmmsg` and `sendmmsg` 32 datagrams at a time: the ceiling for kernel sockets.

| Echoes per second | 16 tasks on one socket | 1 task |
|---|---:|---:|
| giong, epoll | **335,535** | **351,241** |
| giong, io_uring | **319,380** | **346,643** |
| Tokio | 270,672 | 307,195 |
| Kernel × 32 | 389,686 | 366,291 |

Server CPU per echo with 16 tasks: 4.08 µs on epoll and 4.37 µs on io_uring against
Tokio's 5.33 (kernel × 32: 3.55); with one task 3.89 and 4.10 against 4.68 (3.81). Datagrams
that tasks send in one turn of the event loop go to the kernel in one `sendmmsg(2)`, and on
epoll a busy datagram socket leaves the epoll set, so it pays no wake-ups for the datagrams
it sends and receives (`bench/results/udp-echo/vm/epoll-deep/`). This VM varies by about 15%
between rounds: giong on either driver is ahead of Tokio in every round with one task and in
four of five with 16 (in the other, Tokio's 324,148 beat both, also on CPU per echo).

On bare metal (AMD EPYC 4345P, Linux 7.0, the same benchmark over veth, server on one core;
`bench/results/bare-metal-epyc-4345p/linux-7.0/post-soak/udp-udp-epoll-*`), the server core
is saturated near a million echoes a second and the margins are much smaller:

| Echoes per second (median of 5, range) | 16 tasks on one socket | 1 task |
|---|---:|---:|
| giong, epoll | **920,836** (904,872–926,239) | **892,565** (881,529–906,552) |
| giong, io_uring | **902,459** (895,390–914,320) | 875,632 (863,835–927,756) |
| Tokio | 870,488 (855,699–877,000) | 863,001 (851,625–897,835) |
| Kernel × 32 | 1,009,532 | 1,028,443 |

With 16 tasks giong wins clearly: 5.8% (epoll) and 3.7% (io_uring) ahead of Tokio in the
median, ahead in every round, at 1.09 and 1.11 µs of server process CPU per echo against
Tokio's 1.15 (kernel × 32: 0.99). With one task it only ties: ahead in the median (3.4% and
1.5%) but behind Tokio in one round of five on epoll and two on io_uring. Every runtime moves
between two levels from round to round (about 870k and 930k), so differences under about 5%
are within noise. Most of an echo's cost here is the kernel's UDP and veth path: with 16 tasks
giong makes about 0.12 system calls per echo, where Tokio makes a `sendto` for each one
(`post-soak/udp-perf/`).

## QUIC

`bench/quic.sh`, the benchmark that decided how giong-quic is built: the same QUIC engine
(quinn-proto) driven on giong's UDP sockets and on Tokio with quinn-udp, against quinn
itself on Tokio. Each server on one core with the same TLS 1.3 certificate and transport
settings; quinn's client on the other three cores; 8 connections × 16 streams of 100-byte
answers (`rr`), and 4 connections × one stream of 16 MiB answers (`bulk`). A 4-vCPU VM,
five rounds of 5 s, 2026-11-15, before giong-quic's API existed
(`bench/results/quic/cloud-4vcpu/`):

| Server | `rr`: requests/s | CPU per request | `bulk`: Gb/s | CPU per KiB |
|---|---:|---:|---:|---:|
| quinn on Tokio | 158.9k (147–165k) | 6.10 µs | 5.21 | 1,564 ns |
| quinn-proto on Tokio + quinn-udp | 180.6k (169–188k) | 4.92 µs | 5.66 | 1,429 ns |
| **quinn-proto on giong, with UDP offloads** | 176.3k (166–198k) | 5.00 µs | 5.18 | 1,455 ns |
| quinn-proto on giong, epoll | 161.9k (142–171k) | 5.31 µs | 4.62 | 1,451 ns |
| quinn-proto on giong, no offloads | 175.9k (146–197k) | 4.96 µs | 1.88 | 4,325 ns |

- **giong's UDP layer is level with Tokio and quinn-udp** for the same engine: within 2% on
  requests and CPU per request, and within the rounds' ranges on bulk transfers.
- **Against quinn as a whole, 18% less CPU per request** on small streams; bulk transfers are
  level.
- **Offloads are what bulk transfers need**: without GSO and GRO the same engine moves a
  third of the bytes at three times the CPU per KiB. giong-quic uses them on Linux.
- The client was the limit in `rr` (the servers' cores 86–98% busy), so the CPU columns are
  the ones to compare. HTTP/3 end to end, browsers and bare metal are not measured yet.

## Benchmark lab

[`bench/lab`](../bench/lab/README.md) drives giong, Tokio, Monoio and Glommio with the same
server program and a load generator built on none of them (raw epoll, open loop for
latency, corrected for coordinated omission), in interleaved rounds, and marks each row
ahead, level (within 3%) or behind the best of the others. Run on a 4-vCPU cloud VM on
2026-11-21, three rounds of 6 s, at commit 6c163cd (the runtime as of 1.0); the tables
for every scenario and the raw data are in
[`bench/results/lab/cloud-4vcpu/`](../bench/results/lab/cloud-4vcpu/summary.md).

| Scenario | Metric | giong | Best of the others | | |
|---|---|---:|---|---:|---|
| 1 core | requests/s | 156,741 | Monoio 177,325 | 0.88 | behind |
| 1 core | CPU µs per request | 6.3 | Monoio 5.6 | 1.13 | behind |
| 2 cores | requests/s | **386,377** | Monoio 361,477 | 1.07 | ahead |
| 2 cores | CPU µs per request | **5.0** | Monoio 5.5 | 0.91 | ahead |
| 1,000 connections at 20,000 req/s | p99 | **253 µs** | Tokio 265 µs | 0.96 | ahead |
| 8,000 connections at 20,000 req/s | p99 | 278 µs | Tokio 245 µs | 1.13 | behind |
| 8,000 idle connections | KiB per connection | **9.1** | Monoio 12.9 | 0.71 | ahead |
| answers of 1 MB | MB/s | 2,848 | Monoio 2,837 | 1.00 | level |
| uploads of 64 KB | MB/s | 3,013 | Tokio 3,471 | 0.87 | behind |
| uploads of 1 MB | MB/s | 4,408 | Tokio 7,621 | 0.58 | behind |
| 1 request in 20 computes for 1 ms, 2 cores | p99 of the short requests | 1,290 µs | Tokio 780 µs | 1.65 | behind |
| 1% packet loss | p99 | 2.08 s | Tokio 1.76 s | 1.19 | behind |
| 1,000 connections offered 600,000 req/s | answers/s | **147,892** | Monoio 129,376 | 1.14 | ahead |
| the same overload | peak memory | **11.9 MiB** | Monoio 14.9 MiB | 0.80 | ahead |

- **Ahead** with two cores, on idle-connection memory and under overload; **behind** Monoio
  on one core, behind Tokio on large uploads and on short requests mixed with long ones,
  where Tokio's work stealing moves a short request to another core while a long one
  computes and a thread-per-core runtime makes it wait.
- On a shared cloud VM the tails (p99.9) move several times between rounds; the full
  summary lists them, but decide on a bare-metal run.

## Two tenants on one small box

A server in a cgroup of 1 CPU and 128 MiB (one worker and one compute thread, pinned to one
CPU), shared by two tenants. Every request holds memory from its start to its answer and runs
a CPU-bound job on the compute lane. Tenant B sends 100 requests per second and needs each
answered within 50 ms; tenant A keeps 64 (or 128) requests in flight back to back, far more
than the box can serve. Medians of 3 rounds of 20 s, both drivers alike
(`bench/tenants.sh`, raw data in `bench/results/tenants/`, 4-vCPU cloud VM):

| Requests | Service | B in deadline, alone | B in deadline, with A | B p99 with A |
|---|---|---|---|---|
| 2 ms CPU, 256 KiB | no limits | 2,000 | **0** | 10.1 s |
| | one `Budget` for both tenants | 2,000 | **0** | 10.2 s |
| | one `Budget` per tenant | 2,000 | **2,000 (100%)** | 4.4 ms |
| 0.5 ms CPU, 2 MiB | no limits | 2,000 | **0**: OOM-killed in every round | |
| | one `Budget` for both tenants | 2,000 | **0** | 232 ms |
| | one `Budget` per tenant | 2,000 | **2,000 (100%)** | 3.1-4.4 ms |

A limit for the whole service keeps it alive but lets A take all of it; a budget per tenant
(32 MiB and 8 compute closures each, waiting forms only, nothing refused) keeps B at its
solo level while the box stays fully used: A gets the CPU B leaves, and in the first
scenario the box serves about 490 requests per second in total either way. Not measured yet:
more than two tenants, tenants sharing a database pool, and HTTP/1.1 uploads, whose bytes
`giong-http` still counts per worker, not per tenant.

## Build time and size

[`bench/build.sh`](../bench/build.sh): the same three-route JSON service with the same
layers (request ids, a log line per request, compression, a timeout), on giong-web 1.3.0
from crates.io and on axum 0.8 + tokio + tower-http, built on a 4-vCPU cloud VM with
Rust 1.97. Mean of two rounds; times ±10% on a VM, sizes exact. Raw data and the full
table: [`bench/results/build/cloud-4vcpu/`](../bench/results/build/cloud-4vcpu/summary.md).

| | giong-web | axum + tokio + tower-http |
|---|---|---|
| Crates in the build | **22** | 83 |
| Clean debug build | **7.4 s** | 18.8 s |
| Rebuild after a one-line edit | **0.56 s** | 0.87 s |
| `target/` after a debug build | **181 MB** | 403 MB |
| Clean release build | **11.4 s** | 33.9 s |
| Release binary, stripped | **1.45 MB** | 3.60 MB |

For a service this small an edit rebuilds in under a second on both; the gap is in
clean builds (CI, Docker without a cache, a new checkout) and disk. A larger service has
not been measured.

## What has not been measured

- **macOS and Windows.** Both are correct and tested in CI, but not tuned, and no speed
  number for them is published yet: the CI runners are shared virtual machines.
- **HTTP/3 end to end**, against HTTP/2 on giong and against quinn with h3 on Tokio, on bare
  metal.
- **More than eight cores, and the other tables on a physical network.** The bare-metal
  run under [One worker to N](#one-worker-to-n) is one pair of 8-core machines; at 16
  workers neither side was saturated, so a bigger server and a faster load generator are
  still needed. The other tables are loopback on one machine.
- **Disks.** The file benchmark measures cached data only.
- **Newer Monoio and Glommio.** The comparison uses the latest crates.io releases
  (August and March 2024); their main branches may differ.

Reproduce with `bench/run.sh`, `bench/scale.sh`, `bench/workers.sh`, `bench/syscalls.sh`,
`bench/tls.sh`, `bench/fs.sh`, `bench/core.sh`, `bench/postgres.sh`, `bench/mysql.sh`,
`bench/redis.sh`, `bench/http-client.sh`, `bench/web.sh`, `bench/memory.sh`, `bench/compat.sh`,
`bench/udp-echo.sh`, `bench/grpc.sh`, `bench/quic.sh`, `bench/db-path.sh`,
`bench/tenants.sh` (as root, for its cgroup) and `bench/lab/lab.py`; two cores with
`SERVER_CPU=2,3 LOAD_CPU=0,1 WRK_THREADS=2 CONNS=100 OUT=results/2core bench/run.sh 3`.
Recorded output is in `bench/results/`. A change of a percent or two per request is below
the noise of these runs; `bench/instructions.sh` counts the server's instructions per
request with callgrind instead, which repeats within 0.1%. CI holds the hot paths to
their counts: `scripts/instr.py check` fails a change that makes any of 16 operations
(spawning a task, a TCP round trip, an HTTP/1.1, HTTP/2 or TLS request, a database query)
cost 1% more instructions, or an idle connection 1% more heap ([bench/instr](../bench/instr/README.md)).
