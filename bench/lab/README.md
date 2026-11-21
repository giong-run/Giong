# The benchmark lab

This lab measures giong against Tokio, Monoio and Glommio. Every runtime is measured the same way, the raw data is kept, and losses are published next to wins.

```sh
sudo bench/lab/lab.py                      # every scenario, 3 rounds, about 50 minutes on 4 vCPUs
bench/lab/lab.py --only cores,payload      # some scenarios
bench/lab/lab.py --runtimes giong,tokio    # some runtimes
bench/lab/lab.py --quick --rounds 1        # try it: 2-second measurements
bench/lab/lab.py --only soak --soak-secs 3600
bench/lab/lab.py --summary-only --out bench/results/lab/<machine>   # rewrite summary.md from raw.csv
```

The lab needs `cargo`, `python3` and `taskset`. The `loss` scenario needs root, because it uses iptables. The `latency` scenario needs `tc` with `sch_netem` in the kernel. Results go to `bench/results/lab/cloud-4vcpu/` by default; use `--out` to change that, and name the folder after the machine.

## Method

- **The same program on every runtime.** The servers are `bench/giong`, `bench/tokio-raw`, `bench/monoio` and `bench/glommio`. All four use the same HTTP parser, the same buffers and the same routes, so the runtime is the only thing that differs. `giong-epoll` is giong forced onto its epoll driver. Tokio runs with its default multi-threaded, work-stealing scheduler. The others run thread-per-core, with one listener per core.
- **Separate CPUs.** The server is pinned to the last N CPUs and starts one worker per CPU it may use. The load generator is pinned to the remaining CPUs, so the two never compete for a core.
- **A neutral load generator.** `bench/lab/loadgen` is built on raw epoll and threads, so it uses none of the runtimes it measures. It sends HTTP/1.1, one request at a time per connection.
  - *Closed loop* gives throughput: each connection sends its next request when the last answer is in.
  - *Open loop* gives latency at a fixed load. Requests fall due at a fixed total rate, and each latency counts from when its request was due. A server that stalls is charged for every request that waited behind the stall. This avoids coordinated omission, the same correction wrk2 makes.
  - Latencies go into histograms with 128 sub-buckets per power of two (under 0.8% error), up to p99.99.
- **Interleaved rounds.** Within each scenario point the runtimes take turns (A B C D A B C D ...), so drift in the machine's speed hits all of them. The summary shows medians with min and max.
- **Server-side numbers** come from `/proc`:
  - CPU time per request: user plus system time during the measurement, divided by the requests answered;
  - resident memory per idle connection;
  - peak resident memory.
- **Everything is kept:**
  - `raw/` holds one JSON file per measurement, as the load generator printed it;
  - `raw.csv` holds one row per measurement;
  - `conditions.txt` records the machine, kernel, compiler, commit and runtime versions;
  - `summary.md` has the tables and a "where giong stands" list that marks each row ahead, level (within 3%) or behind the best of the other runtimes.

## The matrix

| scenario | what | rows | where it runs |
|---|---|---|---|
| cores | closed loop, `/json`: requests/s and CPU µs per request | 1, 2 cores | any machine with 2N CPUs |
| | | 4, 8, 16+ cores | bare metal (8 to 32+ CPUs) |
| conns | open loop at 20,000 req/s over N connections: p50, p99, p99.9 | 100, 1k, 8k | cloud VM (20,000 file descriptors) |
| | | 30k, 100k | needs 2N file descriptors: bare metal or a VM with raised limits |
| memory | N connections held idle after one request: KiB per connection | 1k, 8k | cloud VM |
| | | 30k, 100k | needs 2N file descriptors |
| payload | closed loop, 16 connections: payload MB/s | answers of 1, 16, 64, 1024 KB; uploads of 1, 64, 1024 KB | cloud VM |
| mixed | open loop, 1 request in 20 computes for ~1 ms (`/compute?n=27`): latency of the short `/json` requests | 1, 2 cores | cloud VM |
| overload | 1,000 connections offered 600,000 req/s: goodput, errors, peak memory; then 20,000 req/s again: did latency recover? | 1 core | cloud VM |
| loss | open loop at 20,000 req/s with 0.1% and 1% of packets dropped both ways (iptables) | | root |
| latency | delay of 1 ms ± 0.5 ms, and Pareto spikes (tc netem) | | needs sch_netem (not on the cloud VM) |
| soak | hours at steady load: memory and latency drift | | `--only soak`, on a dedicated machine |

When a machine cannot run a row (too few CPUs, too few file descriptors, or no netem), the lab skips it and says why.

## Reading the results

- Throughput rows are closed loop. On a small machine the load generator can be what limits throughput; CPU µs per request then says what a request costs the server, whatever load was offered.
- **On a shared cloud VM, the tail (p99.9) moves by several times between rounds**, because one stall of the VM lands there. Treat tail rows from a cloud VM as indicative, and decide on a bare-metal run.
- The `mixed` row is where the scheduling designs differ most. With work stealing (Tokio), a short request can move to another core while a long one computes. With thread-per-core (giong, Monoio, Glommio), a short request waits behind the long one on its core.
- In `overload`, goodput above the recovery rate means the server answered more than it was later asked for. Look at the peak memory and the recovery latency to see whether it held up.
