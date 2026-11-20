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
(file reads go through a blocking thread there, 1.2%). The limit is 1% either way (3% for that
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
| `spawn` | spawn a task that returns at once and await it | 1,405 | 1,124 |
| `yield` | `yield_now`: one trip through the run queue | 501 | 361 |
| `oneshot` | a oneshot channel: create, send, receive | 280 | 280 |
| `channel` | a round trip between two tasks over two bounded channels | 1,764 | 1,484 |
| `timeout` | a timeout around a future that is ready at once | 100 | 100 |
| `tcp-echo` | 64 bytes to an echo task and back over TCP | 3,624 | 4,510 |
| `udp-echo` | a 64-byte datagram to an echo task and back | 4,138 | 4,832 |
| `file-read` | read 4 KiB at an offset of an open file (page cache) | 196 | 248 |
| `http1` | giong-http server, `GET /json` over HTTP/1.1 keep-alive, raw client | 9,410 | 10,276 |
| `http1-client` | giong-http client and server, HTTP/1.1 | 30,511 | 31,561 |
| `h2` | giong-http client and server, HTTP/2 (prior knowledge) | 43,074 | 43,980 |
| `h2-streams` | ten HTTP/2 requests at once on one connection, client and server | 369,419 | 370,252 |
| `https` | giong-http client and server over TLS (rustls), HTTP/1.1 | 42,428 | 43,452 |
| `web` | a giong-web router with a path parameter, raw client | 13,382 | 14,248 |
| `redis-get` | giong-redis `GET` against a fake server | 8,371 | 8,942 |
| `postgres-query` | giong-postgres query, one parameter, one row, fake server | 10,214 | 10,828 |
| `mysql-query` | giong-mysql query, one parameter, one row, fake server | 14,164 | 14,778 |

`udp-echo` went down with the UDP changes of 0.9 (#139, #142): 5,358 to 4,137 on io_uring and
5,031 to 4,783 on epoll, counted with glibc's malloc as all counts were until 2026-11-20, with
one allocation per echo instead of two. On io_uring that includes the check that keeps a direct
send from overtaking queued submissions (Linux 6.0 lost the reply to a datagram sent past a
queued cancellation).

The core changes of #145, measured against the counts before it with the same allocator:
`file-read` 1,489 to 196 on io_uring and 2,637 to 248 on epoll (a read the page
cache can serve is made within the call, with `preadv2(RWF_NOWAIT)`); `tcp-echo`
4,882 to 3,624 on io_uring (a send made at once when nothing else is runnable or queued); on
io_uring `http1` 15.6% fewer, `redis-get`, `web` and `postgres-query` 11% to 13%,
`mysql-query` 8.2%, `http1-client`, `h2` and `https` 5% to 6%, `h2-streams` 2.1%; on epoll
`spawn` 1,541 to 1,124, `yield` 554 to 361 and `channel` 1,862 to 1,484, and the database
cases and `h2-streams` 1.2% to 1.9% fewer. A task is one allocation instead of three; with
the size classes that is 6 instructions of `spawn` on io_uring (1,411 to 1,405). Up:
`yield` 489 to 501 and `channel` 1,732 to 1,764 on io_uring, `udp-echo` +0.5% and +0.6%;
on epoll `tcp-echo` 4,320 to 4,510, `http1` +2.2%, `web` +1.8%, `http1-client` +1.2% and
`https` +0.8%. On epoll a busy worker now looks for I/O every 61 polls instead of only when
its queue runs dry, so a round trip takes more loop turns between `epoll_wait` calls; measured
in time, the epoll round trip got faster (`bench/core.sh` `tcp-rr` 5,741 to 5,304 ns). Each
idle connection takes one allocation fewer (two with TLS); the task and its future now share
one. Its heap grew by 40 to 64 bytes for `idle-http1` and `idle-h2` (1.0% to 2.7%), and moved
by less than 20 bytes for `idle-https`.

### The allocator is not counted as glibc's

A `run` gets its memory from an allocator of the benchmark's own
([`src/alloc.rs`](src/alloc.rs)): power-of-two size classes with a free list each, so an
allocation costs the same instructions whatever happened before. glibc's malloc does not: what
it executes depends on the state of the heap (whether a freed chunk merges with its neighbour,
whether a bin is searched or consolidated), and that state depends on everything allocated
since the process started. The same binary counted 12,304 or 12,620 instructions per
`postgres-query` depending on the length of its own path, all of the difference inside malloc,
and pull requests that had not touched a case failed on it (#142, #158 and #162 on
`postgres-query`, by 2.3% to 2.5%, while the two drivers moved in opposite directions). With
the size classes the same five heap states give the same count to the instruction for
`postgres-query` and `mysql-query` (2.5% and 2.0% apart before) and within 0.02% for `h2`
(0.46% before); those three pull requests move `postgres-query` by +0.11%, -0.06% and +0.30%,
the same on both drivers; and a loop planted in giong-postgres's parser still fails the check
at +2.1%. The numbers are in
[`bench/results/instr/heap-state.txt`](../results/instr/heap-state.txt) and item 5 of
[`conditions.txt`](../results/instr/conditions.txt).

What this leaves out is the time glibc spends inside malloc and free, which is not giong's to
change; what giong can change is how often and how much it allocates, and both still count:
every allocation costs its fixed instructions here, a buffer that grows is copied, and the
allocations per operation are a metric of their own. The memory cases below keep glibc's
malloc, since the resident set they report is the allocator's as much as giong's.

`scripts/instr.py` also runs each case by the same relative path with only giong's own
environment variables (`GIONG_*`): where the stack starts depends on the size of the path and
of the environment, and that alone moved `web` by 0.2%.

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
the other cases', covers both ends, handshakes included. The heap count is exact and
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

The numbers above were measured on a 4-vCPU Xeon VM (Linux 6.18), the instruction counts on
2026-11-20 and the memory on 2026-11-15 and after; what CI
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
