# Where a several-fold speed-up is possible, and where it is not

November 2026, on main `4e7df8b` plus two prototypes. The question: can giong get several
times faster, not a few percent? This note answers from measurements only.

## 1. Plain HTTP over the kernel's TCP: the runtime is 9% of the CPU

From the 0.9 bare-metal rental (`bench/results/bare-metal-epyc-4345p/linux-7.0/release-0.8.0/workers/raw.csv`,
`/json` from the other machine, CPU time per request on the server):

| server, workers | req/s | user µs/req | kernel µs/req | user share |
|---|---:|---:|---:|---:|
| giong, 1 | 567,515 | 0.156 | 1.592 | 9% |
| Tokio (raw), 1 | 557,039 | 0.203 | 1.589 | 11% |
| giong, 8 | 2,935,943 | 0.238 | 2.424 | 9% |
| Tokio (raw), 8 | 2,509,130 | 0.320 | 2.193 | 13% |

- giong's own code is about a tenth of each request's CPU; the kernel's TCP stack, system
  calls and interrupts are the rest. Even a runtime that cost nothing could only make this
  workload about 10% faster. Tokio pays the same kernel cost, so no runtime on the kernel's
  TCP stack can be several times faster than another on it.
- The kernel cost per request grows from 1.59 µs on one worker to 2.42 µs on eight (+52%).
  That growth, not the runtime, is the largest lever left inside the kernel: aligning
  workers with NIC queues (gap-analysis item 7) targets it. At best it buys about 1.4 times
  at eight workers. It needs the bare-metal machines.

## 2. Where multiples are possible

### 2.1 Leaving the kernel's network stack: AF_XDP (prototype, `bench/xdp.sh`)

A UDP echo server on one pinned core, 64-byte payloads, client in another network
namespace over veth, 4-vCPU cloud VM, 5 rounds (`bench/results/xdp/vm/`):

| mode | echoes/s | server CPU µs per echo (process + softirq on its core) |
|---|---:|---:|
| giong `UdpSocket` (io_uring, 16 tasks) | 213,357 | 6.53 |
| Tokio `UdpSocket` (16 tasks) | 278,849 | 5.12 |
| kernel sockets, `recvmmsg`/`sendmmsg` × 32 | 350,418 | 4.06 |
| **AF_XDP**, copy mode | **765,535** | **2.04** |

- AF_XDP turns frames around in user space after the driver, skipping the IP/UDP stack and
  the socket layer. It echoes 3.6 times as many packets as giong today, 2.2 times as many
  as the best use of kernel sockets, at a third of giong's CPU per packet.
- This is a lower bound for the gain: copy mode on veth copies every frame twice and needs
  a system call per batch to transmit. A real NIC in zero-copy mode avoids both. It is also
  an upper bound for the effort it measures: no IP fragments, no ARP (fixed neighbours),
  one queue. A real path needs one AF_XDP socket per NIC queue and worker, ARP and ICMP
  handed back to the kernel (`XDP_PASS`), and a fallback where XDP is not allowed.
- It fits QUIC first: QUIC already runs its own transport in user space over UDP, so
  giong-quic would gain the full step without a new TCP stack. A user-space TCP stack for
  HTTP/1.1 and HTTP/2 is a far larger project (retransmission, congestion control,
  interoperability with every client); it should start only if the UDP path shows the
  multiple on a real NIC.
- **Found on the way:** giong's per-datagram UDP path is slower than Tokio's here
  (213,357 vs 278,849 echoes/s; 252,008 on giong's epoll driver in a single run). That is
  a fixable gap in the current runtime, independent of AF_XDP.
  **Fixed since** (branch `perf/udp-path`, `bench/results/udp-echo/`): 330,254 echoes/s on
  io_uring with 16 tasks and 350,457 with one, 302,000 on epoll, against Tokio's 286,618
  and 276,996.

### 2.2 Latency when the CPU is contended (prototype on `feat/sched-contract`)

Workers ask the kernel for a 100 µs scheduler slice; compute threads run in `SCHED_IDLE`.
4-vCPU VM, 5 rounds, another program spinning on every CPU: p99 echo latency 955 µs before,
590 µs after, Tokio 3,724 µs (`bench/results/sched-contract/` on that branch). A C
microbenchmark of the same effect: 1,889-1,943 µs with the default slice, 77-630 µs with a
100 µs slice. The bare-metal case it targets (compute lane p99 1,121 µs at nice +10) does
not reproduce on the VM and needs a bare-metal run.

### 2.3 Multiples already measured

| what | gain | where |
|---|---:|---|
| receive bundles, bulk, 8-64 streams | 2.4× faster, −58% CPU | draft #117, bare metal |
| `splice_to` proxy, 256 streams | 2.7× faster, −64% CPU | draft #117, bare metal |
| QUIC, 256 connections, one endpoint per worker | 5.9× quinn, 3.9× quinn-proto | draft #109, bare metal |
| slow-upload flood in 512 MiB | 79 MiB peak vs axum 498 MiB (killed) | 0.9 small box |

## 3. Open: QUIC `rr` p99 53 ms on bare metal

Run over veth with GRO enabled on both ends (`bench/quic` from #109, 8 connections × 16
streams), giong with offloads gives 213,248 req/s, p99 1.3 ms, the same as quinn-proto on
Tokio: the client was the limit and the stall did not appear. The stall needs the real NIC.
Leading suspects, in the order to check them: datagrams lost when a coalesced GRO
receive or the socket receive buffer overflows (QUIC then waits for its probe timeout,
about 25 ms, twice), and giong's 1 ms timer resolution against the sub-millisecond
deadlines quinn-proto asks for at bare-metal round-trip times. Checking both needs
`netstat -su` receive errors and a `udp:udp_fail_queue_rcv_skb` count during one bare-metal
run.

## 4. What to do with this

1. Fix the per-datagram UDP gap (2.1, last point): runtime work, measurable in the cloud.
2. Bare-metal runs after the 72-hour soak: `bench/xdp.sh` on the NIC in zero-copy mode,
   `bench/lanes.sh` on the scheduler branch, and the QUIC `rr` checks in section 3.
3. If zero-copy AF_XDP holds a several-fold gain on the NIC, an AF_XDP UDP path for
   giong-quic in 1.3, behind the same `UdpSocket`-shaped API, with the kernel socket as the
   fallback. A user-space TCP stack only after that, as its own decision.
