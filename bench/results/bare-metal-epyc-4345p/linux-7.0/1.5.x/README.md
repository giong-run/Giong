# Bare metal, third rental (1.5.x): results

Two Vultr bare-metal machines in Singapore, rented for one day on 2026-11-27 to test the 1.5.x
line with evidence: regression, A/B of the 1.5.x changes, rate sweeps, whole-service gzip,
a million connections, HTTP/3, the lab, and soaks. They are the same model as the two earlier
rentals, but not the same physical machines.

| | |
|---|---|
| Machines | 2 × AMD EPYC 4345P (8 cores, 16 threads), 64 GB, Intel E810 25 GbE (`ice`), 2 × NVMe |
| Kernel | Ubuntu 24.04.5 with the HWE kernel 7.0.0-44-generic, booted once for the run (`boot7.sh`); `irdma` unloaded, 16 combined queues, EPP `performance` (`tune.sh`) |
| Network | server br-1 (10.40.96.3), load generator br-2 (10.40.96.4), over the private 25 GbE network (VPC, MTU 1450) |
| Rust | 1.97.0 |
| Scripts | `../scripts-1.5/` (`run.sh`, `local.sh`, `ab-net.sh`, `rate.sh`, `gzip.sh`, `h3.sh`, `c1m.sh`, `soak.sh`) |
| Builds | `main` = 14e0db9 (1.5.2 and what followed) unless a folder says otherwise; v1.5.1 = 7f56de7; #487 = ab4a345; `mainnow` = f265c0c |

`br-1/out/<run>/` and `br-2/out/<run>/` hold each run as the scripts wrote it: `conditions.txt`
(machine, commits, settings), `raw.csv` or `raw/` (every measurement), `summary.txt` or
`summary.md`, and the logs. `br-*/progress.log` is the run's timeline. The kernel logs kept
during the soaks held only the firewall's records of internet scanners on the public interface,
so they are replaced by a line counting them (no other kernel line appeared).

Every number below is a median of rounds; the spread is in each folder. Higher is better for
requests per second and MB/s, lower for CPU, memory and latency. Losses are in bold.

## Summary

| | result |
|---|---|
| Regression against 0.8.0 | giong moved −4.3% to +2.7% and giong-http −2.7% to +4.4% across 1–16 workers, while Tokio, run alongside, moved −4.6% to +2.4% (−4.0% at eight workers, where giong moved −4.3%): machine differences, no regression |
| HTTP/2 over TLS, one core | 1.66M req/s, **4.8×** axum-tls (347k); `json-mux` 2.42M, 7.0× |
| Idle memory per connection | HTTPS 10.92 → 5.28 KiB (**−52%** since v1.5.1), h2 over TLS 17.51 → 6.62 KiB (**−62%**), h2c 4.68 → 2.66 KiB (9× less than axum) |
| A million connections | 1,000,000 held over HTTP/1.1, h2c, HTTPS and h2 over TLS, 0 errors; 1.32 / 2.55 / 5.17 / 6.46 KiB per connection on the server |
| HTTP/3 | 396k req/s on one worker against 308k for quinn; 1.37M on four against 816k |
| Whole-service gzip (#500) | `/users` +46% req/s, −31% CPU per request |
| Uploads in the lab (#510) | 1 MiB: 9.2 → 17.2 GB/s, level with Tokio (17.3) |
| PR #499 | `/json` on one worker +7.4% (gap to Tokio −12.2% → −5.7%), four workers +5.4%; no tail loss once measured again |
| Soaks | 3 h core (io_uring and epoll), 2 h HTTP/3, TLS and gRPC: all passed, descriptors back to their start |
| **Losses** | giong-http on one worker −5.7% against Tokio's hand-rolled loop even with #499 (−12% without); QUIC request/response 498k against quinn's 680k; QUIC bulk 10.5 against 14.8 Gbit/s on one worker; lab uploads of 64 KiB 0.75 of Tokio; lab answers of 64 KiB p99 220 against 126 µs; lab `mixed` p99.9 505 against Glommio's 378 µs; lab 1–2 cores 5% behind Monoio; kTLS slower than userspace TLS for small answers; #180 `mixed` p50 +80% |

## 1. Regression against 0.8.0 (`br-1/out/reg/`)

`/json` from the other machine, requests per second. The 0.8.0 column is from the second
rental (`../release-0.8.0/`, other physical machines, rustc 1.100.0); Tokio's columns show how
far the two setups differ on their own.

| workers | giong 0.8.0 | giong now | giong-http 0.8.0 | giong-http now | Tokio (raw) 0.8.0 run | Tokio (raw) now |
|---:|---:|---:|---:|---:|---:|---:|
| 1 | 567,515 | 559,291 | 471,872 | 468,056 | 557,039 | 531,323 |
| 2 | 1,061,047 | 1,023,847 | 873,173 | 887,099 | 951,942 | 953,921 |
| 4 | 1,780,009 | 1,828,084 | 1,492,347 | 1,558,491 | 1,588,063 | 1,616,156 |
| 8 | 2,928,580 | 2,801,942 | 2,620,177 | 2,549,145 | 2,522,580 | 2,420,620 |
| 16 | 3,174,155 | 3,215,975 | 3,081,614 | 3,163,683 | 2,817,873 | 2,884,467 |

Monoio (tuned) ran too: 525,650 on one worker, 2,743,547 on eight. The io_uring features
(`reg/uring-features/`) give the same picture as before: without zero-copy send, CPU per
request is 18% higher for 64 KiB files; the other features move it by less than ±2%.

## 2. A/B: v1.5.1, main and #487 (`br-1/out/ab/`)

The builds took turns over the network, 6 rounds, one worker. Against v1.5.1, `main` moved
−2.2% (`h2:json`) to +1.6% (`http:json`), and #487 −1.1% to +2.5%. TLS handshakes were
−1.3% (main) and −1.1% (#487). The HTTP/2 loss on `main` did not repeat in the second A/B
(section 10: `h2:json` +0.2%, `h2c:json` +2.7%), so it is within the spread.

## 3. Rate sweep: giong-http against a hand-rolled Tokio loop (`br-1/out/rate/`)

Closed loop, 64 connections per worker:

| workers | giong-http `/json` | tokio-raw `/json` | giong-http `/users` | tokio-raw `/users` |
|---:|---:|---:|---:|---:|
| 1 | **464,053** | 527,271 | **110,105** | 114,236 |
| 2 | **865,830** | 938,192 | **215,030** | 221,076 |
| 4 | 1,574,215 | 1,599,861 | 351,238 (25 Gbit/s) | 344,420 |
| 8 | 2,545,151 | 2,452,095 | 351,239 (25 Gbit/s) | 351,054 |

Open loop, the highest rate answered with p99 under 5 ms: on eight workers giong-http held
2,000,000 req/s at p99 653 µs, where Tokio's p99 was 3.8 ms. On one worker, giong-http costs
2.15 µs of CPU per request against 1.89: the gap PR #499 works on (section 10).

## 4. Whole-service gzip, #429 / #500 (`br-1/out/gzip/`, `gzip-json1/`)

`bench/giong-web` with `Compression::new()`, `/users` (8,347 bytes of JSON):

| workers | load | req/s miniz → fast | CPU µs/req | p99 µs |
|---:|---|---|---|---|
| 1 | closed | 28,967 → 42,293 (+46.0%) | 34.47 → 23.62 (−31.5%) | 2,818 → 1,942 |
| 1 | open, 80% | same rate | 34.98 → 24.31 (−30.5%) | 346 → 228 |
| 4 | closed | 112,356 → 161,176 (+43.5%) | 35.53 → 24.75 (−30.3%) | 2,908 → 2,003 |

`/json` (never compressed) is unchanged. At the one-worker knee (80% load), `/json`'s p99 was
much worse with `fast` in one run and much worse with `miniz` in the recheck (`gzip-json1/`):
both builds run the same code there. Shipped as the opt-in `fast-gzip` feature (#500).

## 5. A million connections (`br-1/out/c1m/`)

1,000,000 connections held open, then probed with requests while held:

| build | mode | errors | server KiB/conn | + kernel KiB/conn | probe req/s before / held |
|---|---|---:|---:|---:|---|
| main | http1 | 0 | 1.39 | 5.37 | 1,001,800 / 1,036,850 |
| main | https | 0 | 5.31 | 9.06 | 949,955 / 951,330 |
| main | h2 over TLS | 0 | 9.68 | 13.49 | 901,114 / 941,070 |
| #487 | http1 | 0 | 1.32 | 5.09 | 1,036,690 / 1,081,370 |
| #487 | h2c | 0 | 2.55 | 6.35 | 1,041,730 / 1,031,790 |
| #487 | https | 0 | 5.17 | 8.91 | 917,700 / 954,123 |
| #487 | h2 over TLS | 0 | 6.46 | 10.18 | 919,886 / 944,287 |

Holding a million idle connections cost no throughput and no idle CPU (0.00%).

## 6. HTTP/3 and QUIC (`br-1/out/h3/`, `quic/`)

| workers | case | giong | quinn |
|---:|---|---:|---:|
| 1 | `json` | 395,733 | 307,611 |
| 1 | `users` | 131,163 | 130,559 |
| 4 | `json` | 1,371,604 | 815,949 |
| 4 | `users` | 332,118 | 263,239 |

Raw QUIC (`quic/`): many short connections, four workers, 1.50M against 341k. **Losses:**
request/response on one connection, 498k against quinn's 680k (giong's server at 62% CPU, so
not CPU-bound: a pacing or flow-control limit to find), and bulk transfer on one worker,
10.5 against 14.8 Gbit/s (15.4 against 16.0 on four).

## 7. One machine: HTTP/2, TLS, files, the HTTP client, memory

On br-1 alone (`local.sh`), server on one core:

- **HTTP/2** (`h2/`): 1.66M req/s over TLS against axum-tls's 347k; `users` 132k against 49k.
  Idle h2c connection 3.47 KiB against axum's 23.79.
- **TLS** (`tls/`): `/json` 393k against axum-tls's 304k (2.54 against 3.28 µs/req),
  handshakes 17.2k against 14.6k, `users` 2×. **kTLS is slower for small answers** (`/json`
  332k, `bytes1m` 4.2k against 5.3k) and faster only for files (`file1m` 6.2k against 4.7k); it
  stays off by default.
- **Files** (`fs/`): random reads 2.88M ops/s on one task (std 2.95M, Tokio 319k); small files
  806k (Tokio 280k). **Small files are 22% behind blocking std on one task.**
- **HTTP client** (`httpc-main/` against `httpc-v151/`): h2 `/json` 724k → 787k (+8.8%), http
  376k → 390k, 2.5× reqwest. **h2 `users` costs the client 13% more CPU per request** (2.93 →
  3.30 µs) while the server is the limit; not checked further.
- **Memory** (`mem/`): idle HTTPS 10.92 → 5.28 KiB per connection (v1.5.1 → #487), h2 over TLS
  17.51 → 6.62, h2c 4.68 → 2.66.

## 8. Spin governor and placement (`br-1/out/spin-*/`, `placement/`)

- The spin governor (#186, closed): `tuned` costs nothing at low load (0.3% CPU at 1,000 req/s
  against 20% for a fixed spin) and spins like the fixed mode from 5,000 req/s, for p50 −1 µs.
- Placement by CPU time (#180): skew-paced p99 330 → 59 µs, hot worker +13%; **in `mixed`
  the light requests' p50 went 158 → 285 µs (+80%)**, so #180 stays a draft.

## 9. The lab (`br-1/out/lab/`, `lab-rest/`, `lab-ri/`, `lab-ri-main/`)

`bench/lab/lab.py` with SMT off. `lab/` holds cores and conns, `lab-rest/` the other scenarios
(memory, payload, mixed, overload, loss, latency), and `lab-ri/` the payload scenario again
with #510.

- Four cores: giong 2.00M req/s, p99 134 µs; Tokio 1.68M, p99 333; Monoio 1.96M.
  **One and two cores: 5% behind Monoio** (488k against 516k).
- Idle memory: 9.0–9.2 KiB per connection from 1,000 to 100,000 connections; Monoio 12.9,
  Tokio 13.1.
- Overload (1,000 connections offered 600,000 req/s): 512k answered against Monoio's 494k and
  Tokio's 463k, with 20% less peak memory.
- Uploads, with #510's `recv_into` in `bench/giong`: 1 MiB 9,202 → 17,192 MB/s (Tokio 17,324),
  CPU 114 → 61 µs per upload, p99 2,036 → 1,006 µs (Tokio 1,749). **64 KiB uploads: 10,075 →
  11,267 MB/s, still 0.75 of Tokio's 15,072.**
- **Answers of 64 KiB: p99 220 µs against Tokio's 126** at the same throughput (0.96).
- **`mixed` (1 request in 20 computes for ~1 ms): p99.9 505 µs against Glommio's 378.**
- Packet loss and added delay: level with the others.
- The 30,000-connection point of `conns` is not usable: the connections did not finish opening
  within the warm-up (giong answered 91% of what was sent, Tokio 8%), and Monoio failed to start
  after it, which ended that lab run. The latency scenario's second shape was refused by `tc`
  (`delay 0ms 10ms distribution pareto` has no base delay); `lab.py` now gives it 1 ms.

## 10. PR #499 (`br-1/out/rate-pr499/`, `ab-h2-pr499/`, `rate-pr499-tail/`, `bis-users/`)

| run | main | #499 | Tokio (raw) |
|---|---:|---:|---:|
| `/json`, 1 worker, closed | 454,801 | 488,417 (+7.4%) | 518,001 |
| `/json`, 4 workers, closed | 1,532,788 | 1,615,935 (+5.4%) | 1,579,922 (earlier run) |
| `h2c:json`, 1 worker | 1,712,486 | 1,844,805 (+7.7%) | |
| `tls:json`, 1 worker | 370,253 | 389,628 (+5.2%) | |

The first rate run had #499's p99.9 at 965 µs against 347 for main at 1M req/s on four
workers. Run again (`rate-pr499-tail/`, 6 rounds) it was 350 against 358: both builds have
single-round spikes (#499 10.1 ms once, main 825 µs once). The `http:users` p99 of #499 was
765 against 712 µs in `ab-h2-pr499/`; a bisect over its commits (`bis-users/`) found the p99
at about 700 µs in some rounds and about 765 µs in others for every build, the 1.5.3 base and
main included (medians: base 736, main 762, #499 763). Neither is a loss of #499.

## 11. Soaks (`br-2/out/soak-phase2/`, `br-2/out/soak/`, `br-1/out/soak/`)

| soak | length | work | RSS start → end | descriptors |
|---|---|---|---|---|
| core, io_uring | 3 h | 241M connections, 542M echoes | 10.3 → 15.5 MiB | 296 → 4 |
| core, epoll | 3 h | 239M connections, 538M echoes | 8.5 → 8.6 MiB | 296 → 4 |
| HTTP/3 | 2 h | 2.52M connections, 11.3M requests | 78.7 → 77.9 MiB | 74 → 3 |
| TLS | 2 h | 20.8M connections, 39.6M echoes | 66.1 → 68.8 MiB | 167 → 16 |
| gRPC | 2 h | 20.0M unary, 28.8M streams, 1.41G messages | 18.2 → 18.8 MiB | 116 → 20 |

All passed with no error. The io_uring soak's growth is not a leak: it steps when the receive
pool lends more buffers (1 MiB, then 2 MiB, at most 8 MiB), and grows by 4 KiB every 256 s on
both drivers because the soak keeps its own samples (16 bytes a second).
