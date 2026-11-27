# giong-web `fast-gzip`: the whole service on bare metal

The `fast-gzip` feature (contributed by concacotdua in
[#429](https://github.com/giong-run/Giong/pull/429)) has `Compression` gzip bodies of up to
16 KiB at level 1 with libdeflate instead of miniz_oxide. This measures what it changes for a
whole service: `bench/giong-web` with `MIDDLEWARE=1` (request ids, `Compression::new()`, a
timeout), driven over the network by a client that asks for gzip.

## Conditions

- Server br-1, client br-2: two AMD EPYC 4345P (8 cores, 16 threads, 3.8 GHz), 25 GbE
  between them, Linux 7.0.0-44-generic, the tuning in
  `bench/results/bare-metal-epyc-4345p/linux-7.0/scripts-1.5/tune.sh`.
- Both builds come from PR #429 at 03b79ca with rustc 1.97.0, `cargo build --release` in
  `bench/giong-web`: `miniz` with the default features, `fast` with `--features
  giong-web/fast-gzip` (now also `--features fast-gzip`). The one local change made it listen
  on `BENCH_HOST`, which `bench/giong-web` now reads itself.
- The server on the first 1 or 4 of CPUs 0-7 (`taskset`), `GIONG_WORKERS` unset (one worker
  per allowed CPU). The client, `bench/lab/loadgen`, uses 16 threads, 64 connections per
  server worker and `Accept-Encoding: gzip`, 10 s per measurement after 2 s of warm-up.
- Each round runs both builds in turn, the order rotating from round to round: a closed loop
  (as fast as answers come), then an open loop at 50% and at 80% of the `miniz` build's
  closed-loop rate of the same round, the same rates for both builds. 6 rounds; the tables
  give medians over rounds.
- `/users` is 8,347 bytes of JSON: `miniz` sends 1,114 bytes of gzip, `fast` 1,068 (−4.1%).
  `/json` is 27 bytes, under `min_size`, never compressed: both builds should match there.
- Script: `bench/results/bare-metal-epyc-4345p/linux-7.0/scripts-1.5/gzip.sh`, run as
  `OUT=/root/r15/out/gzip bash gzip.sh 6 10`; the `/json` recheck as `WORKERS=1 PATHS=/json
  OUT=/root/r15/out/gzip-json1 bash gzip.sh 6 10`.

Files: [`first-run/raw.csv`](first-run/raw.csv) (every measurement, in the order run),
[`first-run/summary.txt`](first-run/summary.txt),
[`first-run/conditions.txt`](first-run/conditions.txt); the same three in
[`json-recheck/`](json-recheck/).

## Results

`/users`, gzip on (medians of 6 rounds; CPU per request is the server's CPU time over the
answers):

| workers | load | miniz req/s | fast req/s | change | CPU µs/req miniz → fast | p99 µs miniz → fast | RSS+ KiB miniz → fast |
|---:|---|---:|---:|---:|---|---|---|
| 1 | closed | 28,967 | **42,293** | **+46.0%** | 34.47 → **23.62 (−31.5%)** | 2,818 → **1,942** | 1,192 → 936 |
| 1 | open 50% | 14,483 | 14,483 | | 35.00 → **24.08 (−31.2%)** | 254 → **209** | 1,174 → 946 |
| 1 | open 80% | 23,173 | 23,173 | | 34.98 → **24.31 (−30.5%)** | 346 → **228** | 1,200 → 970 |
| 4 | closed | 112,356 | **161,176** | **+43.5%** | 35.53 → **24.75 (−30.3%)** | 2,908 → **2,003** | 4,300 → 3,600 |
| 4 | open 50% | 56,176 | 56,176 | | 35.75 → **24.41 (−31.7%)** | 308 → **265** | 4,242 → 3,730 |
| 4 | open 80% | 89,881 | 89,882 | | 35.89 → **24.63 (−31.4%)** | 594 → **350** | 4,370 → 3,820 |

p50, p99.9 and the slowest request are lower too at every load (`summary.txt`). RSS+ is
the server's resident memory after the measurements minus before them: the libdeflate
compressor does not cost more memory than miniz_oxide's pooled ones, it costs less.

`/json`, never compressed: closed loop +0.1% (1 worker) and +0.5% (4), CPU per request
−0.1% and −0.5%, p99 within 9 µs at every load but one. The exception is one
worker at 80% load, which is the knee of `bench/giong-web` on one worker: its p99 jumps
between 2 ms and 600 ms from round to round with either build. The first run gave
`miniz` 9.9 ms and `fast` 79 ms (medians); the recheck, the same measurement 6 rounds
again, gave the opposite, `miniz` 69 ms and `fast` 7.2 ms. A build that never compresses
`/json` runs the same code for it either way, so this is the knee's noise and not the
feature. At 4 workers three rows of the 80% load (two `miniz`, one `fast`) fell behind the
offered rate with a p99 over 1 s; the median there is 323-328 µs for both.

What `fast` costs:

- The binary is 88 KiB larger (1,579,064 against 1,489,072 bytes), and the server starts
  with about 120 KiB more resident memory (median before any load: 3,352 against 3,236 KiB
  on one worker, 3,976 against 3,854 on four), libdeflate's code. Where nothing is
  compressed (`/json`) the peak is that much higher too: 4,116 against 4,048 KiB on one
  worker, 6,842 against 6,776 on four. Where bodies are compressed the peak is lower
  (4,306 against 4,434 and 7,692 against 8,156).
- `/json` CPU per request moved between −0.5% and +1.2% across loads and runs (the recheck's
  closed loop: −0.8% requests per second, +0.9% CPU per request; the first run's: +0.1%
  and −0.1%). Both builds run the same code for `/json`; this is the run-to-run spread.
- Building needs a C compiler (libdeflate-sys compiles libdeflate with `cc`).

## After the run

The PR adopting #429 changes one thing in the measured code: libdeflate writes into a
buffer each worker keeps (it grows once, to about 16 KiB) and the answer is copied out at
its exact length, instead of a new zeroed buffer of the worst-case size for each response
that the answer then kept as spare capacity. Counted by callgrind on the 8,347-byte `/users`
body (`gzip` alone, 1,000 calls, release build, 4-vCPU cloud VM): 249,030 → 240,802
instructions per call (−3.3%), and each compressed body holds 1,068 bytes instead of about
8.4 KiB of capacity. Not measured again on bare metal.
