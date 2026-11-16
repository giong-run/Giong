# Bare metal, second rental (0.9): results

Two Vultr bare-metal machines in Singapore, rented again for milestone 0.9 ("Bare metal
and real use") on 2026-11-16. They are the same model as the first rental, but not the
same physical machines. The folders `fs`, `ktls`, `run`, `scale`, `tls`, `workers`,
`release-0.5.0` and `machine.txt` are from the first rental (2026-11-13/14). Everything
listed below is from the second.

| | |
|---|---|
| Machines | 2 × Supermicro AS-3015MR-H8TNR, AMD EPYC 4345P (8 cores, 16 threads, boost on), 64 GB, Intel E810-XXV 25 GbE |
| Kernel | Ubuntu 24.04.5 with its HWE kernel 7.0.0-43-generic, `irdma` unloaded, 16 combined queues on both ports, EPP `performance` (`machine-0.9.txt`) |
| Network | server br-1 (10.40.96.4), load generator br-2 (10.40.96.3), over the private 25 GbE network (VPC) |
| Rust | 1.100.0; wrk 4.1.0, h2load 1.59.0, Go 1.22.2, nginx 1.24.0 with OpenSSL 3.0.13 (Ubuntu's packages) |
| Scripts | `scripts-0.9/`: `setup-base.sh` (machine setup), `reg09.sh` (regression pass), `extras09.sh` and `lanes2.sh` (draft branches), `fixcheck.sh` and `h2mem-ab.sh` (A/B runs), `soak72.sh` (72-hour soak) |

Every number below is a median of rounds. The raw data and the spread are in each folder.
Lower is better for CPU and memory, higher is better for requests per second.

## 1. Regression: 0.8.0 against 0.5.0 (`release-0.8.0/`)

giong 0.8.0 (main `b6d9b4b`), measured with the same scripts and settings as the final
0.5.0 pass (`release-0.5.0/`, commit `d5a0b0c`, first rental).

Worker scaling, `/json` from the other machine (`workers.sh 3 6`), requests per second:

| workers | giong 0.5.0 | giong 0.8.0 | giong-http 0.5.0 | giong-http 0.8.0 | Tokio (raw) 0.8.0 run |
|---:|---:|---:|---:|---:|---:|
| 1 | 559,759 | 567,515 | 457,790 | 471,872 | 557,039 |
| 2 | 1,060,965 | 1,061,047 | 858,329 | 873,173 | 951,942 |
| 4 | 1,752,238 | 1,780,009 | 1,489,013 | 1,492,347 | 1,588,063 |
| 8 | 2,942,600 | 2,928,580 | 2,601,521 | 2,620,177 | 2,522,580 |
| 16 | 3,238,091 | 3,174,155 | 3,115,642 | 3,081,614 | 2,817,873 |

From 8 workers on, the load generator (an identical machine) is part of the limit, as in
the first rental. Every change is within ±2%.

On one server core over loopback:

| | 0.5.0 | 0.8.0 | change |
|---|---:|---:|---|
| HTTP/2 `json`, TLS (h2load) | 1,038,528 | 1,421,176 | +37% |
| HTTP/2 `json-mux`, TLS | 1,507,440 | 1,923,380 | +28% |
| HTTP/2 `users`, TLS | 112,206 | 112,246 | same |
| HTTPS `json` (wrk) | 400,466 | 382,841 | **−4.4%** |
| HTTPS `users` | 100,570 | 99,184 | −1.4% |
| HTTPS `bytes1m` | 5,044 | 4,942 | −2.0% |
| kernel TLS `file1m` | 6,073 | 6,077 | same |
| idle HTTP/2 connection, KiB | 4.21 | 6.78 | **+61%** |

The two regressions in bold are worse than hoped. The HTTPS `json` drop is close to the
spread between two physical machines, and it was not measured again on one machine. The
growth in idle HTTP/2 memory is real: run again on one machine, interleaved, 3 rounds
each, it was 4.21 KiB at `d5a0b0c` and 6.78 KiB at `b6d9b4b` in every round
(`h2-idle-ab/`). It was fixed in #125 (section 6).

The io_uring features (`release-0.8.0/uring-features/`) give the same picture as in 0.5.0:
without zero-copy send, CPU per request is 23% higher at 64 KiB and 2.7 times as high at
256 KiB, and the other features move it by less than ±3%.

## 2. The cheapest class of machine: 1 CPU and 512 MiB (`small-box/`)

`bench/small-box.sh 3 10`. Each server runs in its own cgroup with one CPU of quota,
pinned to one logical CPU, `memory.max` 512 MiB and no swap: the size of the smallest VPS
class. One stand-in check on a real VPS of that class has not been done; it needs a
separate rental.

Requests per second from the other machine:

| | giong-http | axum (Tokio) | Go net/http | nginx |
|---|---:|---:|---:|---:|
| `json`, HTTP/1.1 | **466,377** | 390,265 | 215,634 | 429,819 |
| `json-tls`, HTTPS | **377,325** | 329,360 | 185,806 | 264,025 |
| `json-h2`, HTTP/2 over TLS | **1,592,629** | 380,389 | 90,785 | 521,260 |
| `json-tls`, kernel TLS | 328,168 | | | 248,513 |
| `json-h2`, kernel TLS | 1,263,673 | | | 490,997 |

Idle HTTPS connections held in 512 MiB (one TLS handshake and one request each, then
quiet), with the memory each one costs measured at 20,000 connections. The cgroup column
counts the kernel's socket and TLS memory too:

| | most held | how it ended | KiB per connection (cgroup) | of which in the process |
|---|---:|---|---:|---:|
| giong-http, kernel TLS | **69,318** | killed for memory | **7.60** | 3.71 |
| giong-http | 36,398 | killed for memory | 14.42 | 10.50 |
| nginx | 24,029 | killed for memory | 19.13 | 15.42 |
| axum | 17,529 | killed for memory | 29.83 (at 16,000) | 25.96 |
| Go | 16,185 | killed for memory | 32.29 (at 16,000) | 28.16 |
| nginx, kernel TLS | 24,029 | killed for memory | 19.13 | 15.42 |

- Every server, giong 0.8.0 included, was killed by the kernel's out-of-memory killer
  instead of refusing new connections. #125 changed that for giong (section 6).
- nginx's kernel TLS saves no memory per idle connection on Ubuntu 24.04. With OpenSSL
  3.0.13 it hands only the sending side to the kernel (`TlsCurrTxSw` counts it,
  `TlsCurrRxSw` does not), so the receive buffers stay in the process. giong-tls gives
  both directions to the kernel and halves the memory per connection.
- nginx allocates its connection slots up front. With 150,000 slots that costs 64 MiB
  before the first connection.

Slow-upload flood: 1 MB bodies sent one byte short and held, to a handler that reads the
whole body, while a well-behaved client asks for `/json` every 100 ms:

| | uploads sent | outcome | peak memory of the process | well-behaved requests failed |
|---|---:|---|---:|---:|
| giong-http | 3,000 | survived; at most 67 held at once, the rest got `503` (the budget of `max_buffered_request_bytes`) | 79 MiB | 0 |
| nginx | 3,000 | survived; bodies go to temporary files (page cache, which the kernel reclaims); stopped taking new uploads after 1,475 | 89 MiB | 0 |
| axum | 525 | killed for memory after 500 held | 498 MiB | 40 (after the kill) |
| Go | 300 | killed for memory after 275 held | 484 MiB | 40 (after the kill) |

## 3. QUIC prototype, two machines (`quic/`, draft #109 at `0a161d6`)

`bench/quic-2host.sh 3 10`: the server on br-2, the client on br-1, over the 25 GbE card.
It covers the roadmap item "UDP segmentation offload measured in the same rental" (GSO
and GRO here).

| workload | quinn 0.11 on Tokio | quinn-proto on Tokio (1 worker) | giong + offloads, 1 worker | giong + offloads, best worker count |
|---|---:|---:|---:|---:|
| `rr` (8 conns × 16 streams, 100 B), req/s | 695,000 (4 thr) | **1,033,000** | 446,000, p99 up to 27 ms | 271,000 (8 workers) |
| `bulk` (4 × 16 MiB), Gbit/s | 15.5 (8 thr) | 14.0 | 8.9 | **18.7** (epoll, 16 workers) |
| `bulk`, server ns per KiB | 476 | 486 | 630 | 490 (44 segments per send) |
| `many` (256 conns), req/s | 336,000 | 505,000 | 452,000 | **1,984,000** (epoll, 16 workers) |

- With many connections, one endpoint per worker scales: 1.98 million requests per second
  at 16 workers, 5.9 times quinn and 3.9 times quinn-proto on one Tokio thread.
- **Worse than hoped:** with few connections (`rr`) the offload path stalls. It reaches
  less than half of quinn-proto on Tokio, and with 4 or 16 workers its p99 is 53 ms and its
  CPU use 18%. Without offloads (`giong-plain`) it gets 870,000 requests per second with a
  p99 of 285 µs. The cloud VM did not show this. The prototype needs this fixed before its
  numbers mean anything, and `giong-plain` failed the `many` case.
- Bulk with 10 segments per GSO send costs 25–30% more CPU per byte than quinn. 44
  segments per send brings it to quinn's level.

## 4. io_uring pipelines (`pipelines/`, draft #117 at `0967058`)

On one machine, interleaved A/B runs; CPU is the giong worker's (`pipelines/loopback/ab.log`):

| feature | case | before | after |
|---|---|---:|---:|
| receive bundles | bulk, 1 stream: CPU per GiB | 80 ms | 55 ms (−31%) |
| receive bundles | bulk, 8 streams: time / CPU per GiB | 0.92 s / 115 ms | 0.39 s / 47 ms (2.4× faster, −59%) |
| receive bundles | bulk, 64 streams | 7.7 s / 121 ms | 3.2 s / 51 ms (2.4×, −58%) |
| `splice_to` | proxy, 8 streams, vs `read` | 0.150 s / 150 ms | 0.067 s / 64 ms (2.2×, −57%) |
| `splice_to` | proxy, 256 streams, vs `read` | 8.27 s / 258 ms | 3.01 s / 93 ms (2.7×, −64%) |
| `splice_to` on epoll | proxy, 8 streams, vs `read` | 105 ms | 64 ms (−39%) |
| `MSG_RING` wake-ups | ping between two workers | 2.69 µs CPU, 0.244 s | 2.68 µs, 0.258 s (no gain; 6% slower) |
| registered buffers | 4 KiB `O_DIRECT` reads, NVMe, µs per 32 | 26.8 | 23.9 (−11%, spreads do not overlap) |
| registered buffers | zero-copy sends 64 KiB / 256 KiB, µs | 7.78 / 21.7 | 7.44 / 20.9 (−4%) |
| registered buffers | 4 KiB cached reads | 9.96 | 10.16 (+2%) |

Over the network card (`pipelines/http/`, `uring-features.sh 5 8`), receive bundles and
`MSG_RING` change giong-http's `/json` and 16 KiB responses by less than 1%. The proxy and
bulk cases ran over loopback only: running them from the other machine needs changes to
the benchmark.

## 5. Blocking and compute lanes (`lanes/`, draft #118 at `41b356e`)

`bench/lanes.sh 5 10`. An echo server answers 64-byte pings every millisecond while
CPU-bound jobs keep every CPU busy. p99 echo latency in µs:

| | all 16 CPUs (`full/`) | 4 CPUs of quota over 16 CPUs (`quota4-throttle/`) | 4 CPUs of quota on 4 CPUs (`quota4-cpuset4-throttle/`) |
|---|---:|---:|---:|
| giong, no jobs | 7 | 5 | 5 |
| **giong, compute lane (nice +10)** | **1,121** | 8 | 632 (spread 5–863) |
| giong, lane at nice 0 | 1,172 | 7 | 609 |
| giong, jobs on the workers | 2,125 | 1,941 | 1,946 |
| Tokio, no jobs | 7 | 7 | 9 |
| Tokio, `spawn_blocking` | 1,968 | 10 | 1,869 |

- **Worse than hoped:** on bare metal with every CPU busy, the lane's nice +10 does not
  keep the latency down. p99 is 1.1 ms, about the same as at nice 0, against 236 µs (the
  same as with no jobs) on the 4-vCPU cloud VM. It is still 1.8 times better than Tokio's
  `spawn_blocking`, and it completes 3% more jobs than Tokio.
- Under a quota of 4 CPUs with all 16 CPUs to run on, the cgroup was throttled 2,111
  times (10.1 s in all), but the pings kept a p99 of 8 µs: the workers found idle CPUs.
  The weakness the PR names shows in the maximum (2.0–4.9 ms), not in p99. With the
  quota and only 4 CPUs (a container with a cpuset), the lane's p99 swings from 5 to
  863 µs between rounds.

## 6. #125 (refuse connections before memory runs out), checked before the soak (`fix-125/`)

PR #125 at `674c42c`, the same tree as main `003d9e5`, measured on br-1:

| | 0.8.0 | #125 |
|---|---:|---:|
| idle HTTP/2 connection, KiB (3 rounds, interleaved) | 6.78 | **4.23** |
| HTTP/2 with a request pending, KiB | 7.10 | 5.57 |
| idle HTTPS in 512 MiB | killed at 36,398 | **refuses** from 32,734 |
| idle HTTPS in 512 MiB, kernel TLS | killed at 69,318 | **refuses** from 62,225 |
| slow-upload flood, peak memory, failures | 79 MiB, 0 | 80 MiB, 0 |
| `json` / `json-tls`, req/s | 466,377 / 377,325 | 464,024 / 373,581 (−0.5% / −1.0%) |
| `json-h2`, req/s | 1,592,629 | 1,519,045 (**−4.6%**, beyond the spread) |

The lower ceiling is the reserve #125 keeps by default (a tenth of the memory limit). The
HTTP/2 drop is left for a follow-up.

## 7. 72-hour soak (running)

`soak72.sh`: one uninterrupted 72-hour run of `tests/soak.rs` (release build) on
`674c42c`, which is the same tree as main `003d9e5`. It started 2026-11-16 08:56Z:

- br-1: io_uring driver;
- br-2: epoll driver, with the simulation's long runs (`sim/`, growing seed counts and
  simulated days) beside it at nice 19.

The results go here when it ends.
