# Breakthrough candidates, November 2026

What newer kernels and 2024-2026 research offer Gióng beyond what is already
built or planned, ranked by measured or published win against cost, with two
prototypes measured. Read with [2026-11-runtime-gaps.md](2026-11-runtime-gaps.md)
(the gap analysis) and the post-1.0 direction (isolation, diagnostics,
load-aware placement, benchmark lab, adaptive spin governor, end-to-end
batching), which this list does not repeat.

Already tried, not repeated here: AF_XDP (copy mode 3.3x kernel UDP sockets,
zero copy delivered nothing on E810), kTLS (pays only for sendfile of 1 MiB or
more), UDP GSO/GRO, SQPOLL (lost on bare metal: CPU per request +73%),
registered ring fd, fixed files, packed buffer rings, receive bundles and
`MSG_RING` wake-ups (#117), the EEVDF slice scheduler contract (#135).

The ceiling to keep in mind (bare metal, 0.9 data): on plain HTTP the
runtime's own code is about 9% of the CPU per request and the kernel the
rest. No runtime on kernel TCP gets several times faster on plain HTTP;
multiples come only from moving work out of the kernel path (zero-copy
receive, kernel bypass, shared memory) or from doing kernel work closer to
where it is used (locality, in-kernel chaining).

## Ranked shortlist

| # | Candidate | Expected win (evidence) | Kernel / hardware | API fit (1.x, add only) | Cost / risk |
|---|---|---|---|---|---|
| 1 | **Same-host channels over shared memory with io_uring futex wake-ups** | Measured here (cloud VM, 2 processes): busy round trip 0.66 us vs 4.60 us Unix socket (7x) and 8.16 us TCP loopback (12x); streaming 57.2 M vs 19.0 M vs 5.9 M messages/s, CPU per message 0.018 vs 0.101 vs 0.252 us. No gain when both sides sleep (26.6 vs 28.4 us: wake-up latency dominates) | `IORING_OP_FUTEX_WAIT`/`WAKE` 6.7+; epoll driver: eventfd wake | New module (`giong::ipc`), nothing changes | 2-3 weeks. Peer process is untrusted: every index bounds-checked, no pointers in shared memory. Helps only Gióng-to-Gióng traffic on one host |
| 2 | **io_uring zero-copy receive (zcrx)** | Published: single TCP flow +41% (cores apart) / +29% (same core) vs epoll on 200G bnxt; CPU about -40% with 32 KiB receive pages (7.0); mlx5 +12% bandwidth, -20% CPU | 6.15+ (large pages 7.0, queue sharing 6.19); NIC with header split + flow steering (bnxt, mlx5, gve named; ice not named in the sources) | Builder option naming interface and queue; `recv` unchanged | 3-4 weeks plus a bare-metal NIC. The kernel does not configure the NIC (ethtool by the operator); flows not steered to the zcrx queue fall back to copies |
| 3 | **BPF-driven io_uring loop** (`io_uring_bpf_ops`, `loop_step`) | Published: 50-80% faster than linked requests, over 2x a 2-nop link, in the author's benchmark. For Gióng: in-kernel chains (accept then recv, recv then send for proxies, deadline handling) without a return to user space | Merged for 7.1 (applied 2026-03-17); loading struct_ops needs BPF privileges | Opt-in Builder option; programs shipped with the crate | High: privileged, new verifier surface, needs 7.1 in CI (QEMU). Research item; measure an L4 proxy and accept storms first |
| 4 | **Connection steering by receiving CPU** (prototype 1 here) | Measured here: mixed (see below). Published: Cloudflare 2017 got 99% packet locality and no end-to-end gain; the zcrx paper finds syscall and softirq on one CPU more CPU-efficient | Classic BPF on the reuseport group: any kernel, no privilege; needs RSS/RPS spread over the worker CPUs | Builder option, off by default | Small (done as a prototype). Couples worker load to the NIC's hash: imbalance in softirq becomes imbalance in work. Decide on bare metal only |
| 5 | **NAPI busy poll from the ring** | Published: lower latency without spinning in user space; no Rust runtime uses it | io_uring NAPI 6.9 (static tracking 6.13), epoll busy-poll ioctl 6.9; real NIC (no effect on loopback/veth) | Builder option | 1 week, folded into the adaptive spin governor (post-1.0 item 5) as a mode; bare metal only |
| 6 | **NVMe fast path** (O_DIRECT, IOPOLL ring, registered buffers, NVMe passthrough) | Published (VLDB 2026, kernel 6.15): registered buffers +11%, passthrough +20%, IOPOLL +21% on top; PostgreSQL +14% overall | IOPOLL needs NVMe poll queues; passthrough needs a char device and privileges | New `fs` type for direct I/O; separate poll ring per worker (glommio's pattern) | 2-3 weeks plus NVMe bare metal. Only for storage-heavy users |
| 7 | **`IORING_RECVSEND_POLL_FIRST` and small ring flags** | Published claim (same paper): up to 1.5x fewer cycles for receives that would block; small in practice for multishot receive | 5.19+ | Internal | Days; cloud-measurable with bench/instr. 1.0.x internal item |
| 8 | **PGO + BOLT recipe for services** | Runtime user time here is about 6% of server CPU on plain HTTP, so 10-20% faster user code is 1-2% end to end; more for TLS, HTTP/2, gRPC, DB clients | Toolchain only | Docs and a script | Days; low priority |
| 9 | sched_ext policy | Needs root and BPF; the unprivileged EEVDF slice (#135) covers the noisy-neighbour case | 6.12+ | Deployment recipe only | Not a runtime dependency |
| 10 | Kernel bypass TCP over AF_XDP, user interrupts (Skyloft, SOSP 2024) | Multiples for small packets; UINTR preemption at microsecond scale | AF_XDP zero copy on a NIC that works; UINTR not upstream | New transport | Months; watch only |

Lessons from other runtimes (2026):

- **Seastar's asymmetric io_uring backend** (July 2026) moves network I/O of a
  group of shards to one core with SQPOLL. It wins with one shard (+15% over
  linux-aio) and loses at four, where the networking core spends 40-50% of
  its cycles copying. Same finding as our SQPOLL test: do not funnel several
  workers' I/O through one poller core.
- **Apache Iggy** moved from Tokio to thread-per-core on compio (February
  2026): P99.99 latency -81% to -92% at equal throughput, fsync throughput
  +18%. What hurt: `RefCell` borrows held across `.await` (panics), lost
  ordering of unlinked io_uring writes (needs `IOSQE_IO_LINK`), and pure
  shared-nothing failed for metadata; they settled on read-mostly shared
  state written by one shard. For Gióng: a clippy lint recommendation
  (`await_holding_refcell_ref`) in AGENTS.md and the docs, and a read-mostly
  cross-worker state primitive as a candidate library item.

## Prototype 1: connection steering by receiving CPU

`src/net/steer.rs`, turned on with `GIONG_STEER=cpu`. When a worker's
listener starts listening on a shared port, giong attaches a classic BPF
program to the port's `SO_REUSEPORT` group that maps the CPU which received
the SYN to the listener of the worker pinned to that CPU (`SKF_AD_CPU`); any
other CPU falls back to the kernel's hash. No privilege needed. The table
mirrors the kernel's member order (listen order; the last member moves into
a leaving member's place).

Method (`bench/steer.sh`): client in a network namespace behind a veth pair;
RPS on the server side spreads receive processing over the two server CPUs
by flow hash, as RSS would on a NIC; two pinned workers; wrk with 2 threads
on the other two CPUs; `/json`; 5 rounds, order alternating; server CPU time
from `/proc/stat` (softirq included). Cloud VM, 4 vCPU, Linux 6.18.44.
Raw rows: `bench/results/steer/raw.csv`.

| Connections | Variant | req/s (median) | p99 (us) | Server CPU per request (us) |
|---|---|---|---|---|
| 16 | hash | 147,194 | 626 | 10.81 |
| 16 | steer | 150,713 | 1,710 | 10.29 |
| 128 | hash | 233,617 | 2,190 | 7.54 |
| 128 | steer | 218,945 | 1,230 | 7.30 |

Reading: CPU per request -3% to -5%, p99 -44% at 128 connections but 2.7x
worse at 16, throughput -6% at 128. Round-to-round spread is 10-15%, larger
than most of these differences. In this VM softirq is about 70% of every CPU
(RPS uses inter-processor interrupts, which exit to the hypervisor), which
hides any cache-locality gain. With few connections the flow hash puts more
flows on one CPU, and steering then puts both their softirq work and their
requests there. Not a win here; worth one bare-metal run with a multi-queue
NIC before it is dropped or made an option.

## Prototype 2: same-host channels over shared memory

`bench/ipc` (standalone, raw io_uring, no giong internals): two processes,
one io_uring each set up like a giong worker (single issuer, deferred task
running). Shared memory: a single-producer single-consumer ring of 64-byte
slots per direction in a `/dev/shm` file. A side that finds its ring empty
sets a flag that is also the futex word and arms `IORING_OP_FUTEX_WAIT` on
it; the other side wakes it with `IORING_OP_FUTEX_WAKE` only when the flag
was set, and the wake goes in with its own next wait, so a round trip costs
one system call per side when sleeping and none when busy. Sockets use send
and receive submissions through the same kind of ring, one enter per round
trip; in streaming, sockets send 32 messages as one 2 KiB write.

Method (`bench/ipc.sh`): processes pinned to CPUs 0 and 1; ping = one
message in flight (200,000 round trips sleeping, 1,000,000 with 50 us of
spinning before sleeping, like `spin_before_park`); stream = 20,000,000
messages one way. 5 rounds, medians. CPU = both processes. Cloud VM, 4 vCPU,
Linux 6.18.44. Raw rows: `bench/results/ipc/raw.csv`.

| Test | Shared memory | Unix socket | TCP loopback |
|---|---|---|---|
| Round trip, both sleep (us) | 26.6 | 28.4 | 32.2 |
| Round trip, spin 50 us (us) | **0.66** | 4.60 | 8.16 |
| CPU per round trip, spin (us) | **1.31** | 9.04 | 16.07 |
| Stream (messages/s) | **57.2 M** | 19.0 M | 5.9 M |
| CPU per message, stream (us) | **0.018** | 0.101 | 0.252 |

Reading: when both sides are busy, shared memory is 7x faster per round trip
than a Unix socket and 12x faster than TCP, and streaming needs 5.6x less CPU
per message than a Unix socket. When both sides sleep, the wake-up itself
(about 12 us one way in this VM) dominates every transport, so the gain
there depends on the spin governor keeping a busy side awake, not on the
channel. Bare metal will lower the sleeping numbers for all three.

What a `giong::ipc` channel needs beyond the prototype: a handshake that
passes the memfd over a Unix socket (`SCM_RIGHTS`), message framing for
variable sizes (slots chained, or a byte ring), back-pressure that maps to
the bounded-resource rules (a full ring is a coded error or an await, never
growth), validation of every index read from shared memory (the peer is not
trusted), a futex word per direction per worker, and the epoll driver path
(eventfd).

## Recommendation

1. Build `giong::ipc` (candidate 1) as a 1.x item after 1.0.0: it is the
   only candidate that shows a multiple on this hardware today, and it fits
   thread-per-core and bounded resources directly.
2. On the next bare-metal rental, run in one session: steering (candidate 4,
   `bench/steer.sh` with the NIC instead of veth), NAPI busy poll (5), and a
   zcrx check if the NIC supports header split (2). Each is a Builder option
   at most; none changes 1.0 behaviour.
3. Track the BPF io_uring loop (3) once CI has a 7.1 kernel; prototype an L4
   proxy chain on it.
4. Small internal items for 1.0.x: `POLL_FIRST` on receives (7), the clippy
   lint advice from Iggy's experience.

## Sources

- io_uring zero copy Rx, kernel documentation: https://docs.kernel.org/networking/iou-zcrx.html
- Pavel Begunkov, "Evolution of io_uring Zero-Copy Receive", Netdev 0x1A: https://netdevconf.info/0x1A/docs/netdev-0x1a-paper41-talk-paper.pdf
- David Wei, io_uring zero copy rx v13 cover letter (Feb 2025): https://lwn.net/Articles/1009502/
- "BPF comes to io_uring at last", LWN (Mar 2026): https://lwn.net/Articles/1062286/; patch set v4: https://lwn.net/Articles/1056193/; RFC v2: https://lwn.net/Articles/1024361/
- Jasny et al., "High-Performance DBMSs with io_uring: When and How to Use It", PVLDB 19: https://arxiv.org/abs/2512.04859
- ScyllaDB, asymmetric io_uring backend in Seastar (Jul 2026): https://www.scylladb.com/2026/07/22/asymmetric-io_uring-backend-seastar/
- Apache Iggy, thread-per-core io_uring migration (Feb 2026): https://iggy.apache.org/blogs/2026/02/27/thread-per-core-io_uring/
- Cloudflare, "Perfect locality and three epic SystemTap scripts" (2017): https://blog.cloudflare.com/perfect-locality-and-three-epic-systemtap-scripts/
- io_uring_register_napi(3): https://man7.org/linux/man-pages/man3/io_uring_register_napi.3.html
