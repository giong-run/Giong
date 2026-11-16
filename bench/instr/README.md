# Speed and memory thresholds

CI's `instr` job fails a pull request that makes one of giong's hot paths slower or makes an
idle connection cost more memory. It counts the CPU instructions one operation executes, under
callgrind, and the heap an idle connection holds, and compares them with
[`baseline.csv`](baseline.csv):

```sh
scripts/instr.py check      # measure (3 rounds, median) and compare; exit status 1 past a limit
scripts/instr.py update     # measure and write baseline.csv and bench/results/instr/raw.csv
scripts/instr.py measure    # measure only; --cases http1,h2 for some cases, --rounds N
```

It needs valgrind and Rust 1.97.0 (pinned in [`rust-toolchain.toml`](rust-toolchain.toml)):
`sudo apt-get install valgrind` and `rustup toolchain install 1.97.0`. A full check takes
about three minutes, one of them the build.

## Why instructions

On a shared CI runner the time a request takes moves by tens of percent from one run to the
next, so a time threshold would be either noise or blind. The number of instructions it
executes does not depend on the machine's load: in five runs of every case, and three more
with every core kept busy, no count moved by more than 0.07%, except one case on epoll
(file reads go through a blocking thread there, 1.2%) and, since HTTP/2 connections keep
their streams' memory between requests, `h2` (up to 0.6%, inside glibc's malloc, whose
merging of free blocks depends on the order buffers come back; see
[`bench/results/h2-allocs`](../results/h2-allocs/summary.txt)). The limit is 1% either way (3% for that
case), so a change that costs a hundred instructions per HTTP request fails. The measurements
behind these numbers, and a planted regression that the check caught, are in
[`bench/results/instr/conditions.txt`](../results/instr/conditions.txt); every round of the
baseline is in [`bench/results/instr/raw.csv`](../results/instr/raw.csv).

What a count does not see: the kernel's share (system calls and io_uring's work run outside
user space), cache misses and branch mispredictions. A change that trades instructions for
system calls can look better here and be slower; `bench/run.sh`, `bench/syscalls.sh` and the
bare-metal runs remain the measure of speed. This gate catches the common case: code on a hot
path that does more than it did.

## The cases

Each case sets up what it needs, runs 500 operations to warm up, then tells callgrind to zero
its counters (a client request, so nothing that happened before is counted), runs 2,000
operations and has callgrind dump the counters. Everything runs on one thread
(`Builder::block_on`), the client and the server of a case alike, over loopback. The count is
of both ends: the client is giong code too, and the fake database servers cost a few dozen
instructions per request.

| case | one operation | io_uring | epoll |
|---|---|---:|---:|
| `spawn` | spawn a task that returns at once and await it | 1,707 | 1,837 |
| `yield` | `yield_now`: one trip through the run queue | 481 | 546 |
| `oneshot` | a oneshot channel: create, send, receive | 386 | 386 |
| `channel` | a round trip between two tasks over two bounded channels | 1,758 | 1,887 |
| `timeout` | a timeout around a future that is ready at once | 99 | 99 |
| `tcp-echo` | 64 bytes to an echo task and back over TCP | 4,885 | 4,279 |
| `udp-echo` | a 64-byte datagram to an echo task and back | 5,358 | 5,031 |
| `file-read` | read 4 KiB at an offset of an open file (page cache) | 1,478 | 2,938 |
| `http1` | giong-http server, `GET /json` over HTTP/1.1 keep-alive, raw client | 11,144 | 10,022 |
| `http1-client` | giong-http client and server, HTTP/1.1 | 33,947 | 32,654 |
| `h2` | giong-http client and server, HTTP/2 (prior knowledge) | 49,178 | 47,805 |
| `h2-streams` | ten HTTP/2 requests at once on one connection, client and server | 457,788 | 456,636 |
| `https` | giong-http client and server over TLS (rustls), HTTP/1.1 | 46,789 | 45,185 |
| `web` | a giong-web router with a path parameter, raw client | 15,260 | 14,136 |
| `redis-get` | giong-redis `GET` against a fake server | 9,617 | 9,096 |
| `postgres-query` | giong-postgres query, one parameter, one row, fake server | 12,693 | 12,178 |
| `mysql-query` | giong-mysql query, one parameter, one row, fake server | 18,074 | 17,535 |

Each case also records heap allocations per operation (counted by a global allocator in the
benchmark), with the same 1% limit: one more allocation per request fails.

`h2-streams` has a limit of 0.5% on both: its requests arrive in one read and queue up for
their handlers, which `h2` never does, and the fix for idle HTTP/2 memory in 0.9 first gave
that queue back each time it emptied. That cost four allocations and 0.7% more instructions per
operation here (2% per request with ten streams and 4.5% with a hundred under `h2load`, and 4.6%
fewer requests per second on bare metal), inside a 1% limit. Its counts repeat within 0.1%.

Three more cases measure memory, outside valgrind: `idle-http1`, `idle-h2` and `idle-https`
open 2,000 idle connections to a giong-http server in the process (from another thread, with
blocking std sockets, which own no heap) and record how much the live heap and the resident
set grew per connection. For `idle-https` that thread does each TLS handshake and one
keep-alive request with rustls, then drops its TLS state and keeps the socket, so the heap
that stays is the server's (the server cannot tell; nothing is sent). Its allocation count, like
the other cases', covers both ends, handshakes included. Leaving the client thread out would
take a check in the counting allocator, and that alone moved the instruction counts of the
other cases by up to 2% (inlining under LTO), so it was not done. The heap count is exact and
repeats within a byte per connection; its limit is 2%. The resident set moves with the
allocator's choices; its limit is 10%. For `idle-h2` the thread sends the preface, SETTINGS and
one `GET /` on each connection and reads the answer, as `bench/h2.sh` does: until 0.9 it sent no
request, and missed that 0.8.0 kept each connection's memory for streams for a second after a
request (6,852 heap and 7,371 resident bytes per connection on 0.8.0 with the request, against
4,427 and 4,801 with the fix; 2026-11-16). HTTP/2 connections are opened one at a time, each
after the server's answer: opened all at once, the receive buffer pool runs low and the driver
moves bytes to the heap depending on timing, which made the heap per connection vary by 7%.

| case | heap per connection | resident set per connection |
|---|---:|---:|
| `idle-http1` (io_uring / epoll) | 1,940 / 2,102 bytes | 1,995 / 2,169 bytes |
| `idle-h2` (io_uring / epoll) | 4,427 / 4,492 bytes | 4,801 / 4,796 bytes |
| `idle-https` (io_uring / epoll) | 10,784 / 10,821 bytes | 11,123 / 11,086 bytes |

The numbers above were measured on a 4-vCPU Xeon VM (Linux 6.18) on 2026-11-15; what CI
compares against is `baseline.csv`, whose header names the machine it came from.

## When the check fails

The job prints a table of every metric, its baseline and its change, and stores the run's
counts and a ready `baseline.csv` as the run's `instr` artifact.

- **The change is meant to cost or save this much** (a feature on the hot path, an
  optimisation): take `baseline.csv` from the artifact, commit it as
  `bench/instr/baseline.csv` in the same pull request (with `bench/results/instr/raw.csv`
  from `scripts/instr.py update` if measured locally on a comparable machine), and say in the
  description what moved and why. A saving is checked as strictly as a cost, so that the
  baseline always describes `main`.
- **It is not:** find what got slower. Run the case under callgrind and annotate it:

  ```sh
  cd bench/instr && cargo build --release
  valgrind --tool=callgrind --callgrind-out-file=cg.%p target/release/bench-instr run http1
  callgrind_annotate cg.*.1 | head -40
  ```

  The dump ending in `.1` holds the 2,000 measured operations only. Building the commit
  before the change the same way and comparing the two annotations shows where the
  instructions went.
- **The setup changed:** the check refuses to compare when the compiler, valgrind or the
  io_uring features the driver uses differ from the baseline's header, since each changes the
  counts by itself. A new Rust version means a new `rust-toolchain.toml`, a new toolchain in
  the `instr` job of `.github/workflows/ci.yml` and a new baseline, in one pull request.

## Adding a case

A case is an `async fn(&Meter)` in `src/cases.rs` (or `src/db.rs`) that sets up, then runs
`for _ in meter.iter() { one operation }`, and an entry in `CASES`. Run it a few times under
`scripts/instr.py measure --cases <name> --rounds 5`: if the count moves by more than 0.1%,
something in it depends on timing (a timer firing, a pool running low, a read split
differently), and the case needs to avoid it before it can have a limit. Then
`scripts/instr.py update`.

`bench/instructions.sh` measures the server alone, with h2load as the client in another
process; it is for comparing two builds by hand, this crate for the gate.
