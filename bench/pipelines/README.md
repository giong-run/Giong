# io_uring pipelines: what each one buys

Benchmarks for the features that let the kernel do more of the work: waking workers through
their rings (`msg-ring`), receive bundles (`recv-bundles`), `splice_to`, and registered
buffers (measured, not shipped). Each case prints one CSV line; `ab.sh` runs variants
interleaved, starting from a different one each round, appends the raw lines to
`../results/pipelines/raw.csv` and prints the median per variant with its spread.

```sh
bench/pipelines/ab.sh 8 ping 100000   -- on: off:GIONG_IO_URING_OFF=msg-ring
bench/pipelines/ab.sh 5 ping-busy 20000 32 -- on: off:GIONG_IO_URING_OFF=msg-ring
bench/pipelines/ab.sh 6 bulk 1024 8   -- on: off:GIONG_IO_URING_OFF=recv-bundles
bench/pipelines/ab.sh 5 proxy 128 8   -- copy:PROXY_MODE=copy read:PROXY_MODE=read splice:PROXY_MODE=splice
bench/pipelines/target/release/fixed file <file of 512 MiB> 400000 [direct]
bench/pipelines/target/release/fixed send 60000 65536
PATHS="/json /static?size=16384" \
  VARIANTS="all-on: no-recv-bundles:GIONG_IO_URING_OFF=recv-bundles no-msg-ring:GIONG_IO_URING_OFF=msg-ring" \
  OUT=results/pipelines-http bench/uring-features.sh 5 8
```

| case | what it does | CPU counted |
|---|---|---|
| `ping` | two workers bounce a counter over two `giong::sync::mpsc` channels; each waits in between, so every message wakes a sleeping worker | both workers |
| `ping-busy` | the same, with 32 tasks on worker 0 that spin 2 µs and yield: the latency a busy waker adds | both workers |
| `bulk` | `conns` std threads write N MiB each in 64 KiB writes to one worker that `recv`s and drops | the worker |
| `proxy` | `conns` std clients send N MiB each through a one-worker proxy to std sinks; the proxy uses `recv` plus a copy into a `Vec` (`copy`), `read` into its own buffer (`read`) or `splice_to` (`splice`) | the proxy worker |
| `fixed` | raw io_uring, no giong: 4 KiB reads (32 in flight) and zero-copy sends with a plain buffer and with a registered one | the thread |

CPU is `getrusage(RUSAGE_THREAD)` of the giong worker threads, user and kernel: the load
generator is not counted. A time per operation for `bulk` and `proxy` is per GiB.

## Results, 2026-11-15

A cloud VM: 4 vCPU (Intel Xeon @ 2.10 GHz), Linux 6.18.44, loopback. Cloud numbers move by
up to ±15% between runs, so only differences well beyond the spread below count. Medians;
raw lines in `../results/pipelines/raw.csv`, `fixed-buffers.csv` and
`../results/pipelines-http/`.

| feature | case | before | after | change |
|---|---|---:|---:|---|
| `msg-ring` | `ping`, CPU per round trip (8 rounds) | 23.5 µs | 22.6 µs | −4%; time per round trip the same (2.64 vs 2.61 s per 100,000) |
| `msg-ring` | `ping-busy`, p50 / p99 round trip (5 rounds) | 144.5 / 196.9 µs | 144.1 / 193.0 µs | the same: a wake-up sent at the end of the batch costs no latency here |
| `recv-bundles` | `bulk`, 8 streams (6 rounds) | 2.66 GiB/s, 325 ms CPU per GiB | 6.6 GiB/s, 135 ms | 2.5× faster, −58% CPU; no bytes copied out of the pool in either |
| `recv-bundles` | `bulk`, 1 stream (6 rounds) | 190 ms per GiB | 170 ms | −10% CPU; throughput the same (the writer is the limit) |
| `recv-bundles` | giong-http `/json`, 256 connections (5 rounds) | 324,271 req/s, 5.80 µs CPU | 342,754 req/s, 5.43 µs | +5.4% req/s, −6.8% CPU |
| `recv-bundles` | giong-http 16 KiB bodies (5 rounds) | 226,365 req/s | 225,348 req/s | the same |
| `msg-ring` | giong-http `/json` and 16 KiB | | | within ±2% req/s: HTTP does not wake workers across threads |
| `splice_to` | `proxy`, 8 streams (5 rounds) | `read`: 615 ms per GiB, 0.63 s; `copy`: 1,121 ms, 1.15 s | 470 ms, 0.51 s | −24% CPU and +24% throughput against `read`, −58% CPU against `copy` |
| `splice_to` | `proxy`, 1 stream (5 rounds) | `read`: 522 ms per GiB | 428 ms | −18% CPU |
| `splice_to` on epoll | `proxy`, 8 streams (3 rounds) | `read`: 527 ms per GiB | 386 ms | −27% CPU, +26% throughput |
| registered buffers | `fixed`, 4 KiB cached reads (10 runs) | 25.4 µs per 32 | 24.3 µs | −4.5%, inside the spread (22.9-31.6 vs 22.9-30.1) |
| registered buffers | `fixed`, 4 KiB `O_DIRECT` reads, virtual disk (10 runs) | 73.5 µs per 32 | 67.7 µs | −8%, inside the spread (56-85 vs 50-78) |
| registered buffers | `fixed`, zero-copy sends of 64 KiB / 256 KiB (10 runs) | 25.8 / 67.5 µs | 25.0 / 63.0 µs | −3% / −7%, inside the spread |

Registered buffers are not shipped: on the bare operation they save 3 to 8%, inside the noise
of this machine, and a few hundredths of a microsecond of a request that costs several. Using
them would need a second buffer type next to `IoBuf` (memory the runtime owns and registered
with each worker's ring) and pinned memory that the kernel counts against `RLIMIT_MEMLOCK`
on every kernel, the limit `GIONG-E0010` already deals with. Worth measuring again where
they can matter: zero-copy sends over a real network card and `O_DIRECT` reads on NVMe.

## To measure again on bare metal

Loopback copies every byte, never shows a network card's interrupts and runs both ends on
the same 4 cores; the cloud VM's wake-ups go through the hypervisor. On the next bare-metal
pair, run:

- `bulk` with 1, 8 and 64 streams from the other machine (recv-bundles), and
  `bench/uring-features.sh` with the three variants above;
- `ping` and `ping-busy` (msg-ring), where a wake-up does not exit to a hypervisor;
- `proxy` with 1, 8 and 256 streams, client and sink on the other machine (splice_to over a
  real card, which splices pages without the loopback copy);
- `fixed send` over the card, and `fixed file ... direct` on the NVMe disk (registered
  buffers).
