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
| `spawn` | spawn a task that returns at once and await it | 1,100 | 996 |
| `yield` | `yield_now`: one trip through the run queue | 358 | 306 |
| `oneshot` | a oneshot channel: create, send, receive | 278 | 278 |
| `channel` | a round trip between two tasks over two bounded channels | 1,444 | 1,342 |
| `timeout` | a timeout around a future that is ready at once | 98 | 98 |
| `tcp-echo` | 64 bytes to an echo task and back over TCP | 3,196 | 4,134 |
| `udp-echo` | a 64-byte datagram to an echo task and back | 3,712 | 4,454 |
| `file-read` | read 4 KiB at an offset of an open file (page cache) | 190 | 242 |
| `http1` | giong-http server, `GET /json` over HTTP/1.1 keep-alive, raw client | 8,156 | 9,067 |
| `http1-client` | giong-http client and server, HTTP/1.1 | 19,847 | 20,949 |
| `h2` | giong-http client and server, HTTP/2 (prior knowledge) | 28,075 | 29,196 |
| `h2-streams` | ten HTTP/2 requests at once on one connection, client and server | 222,155 | 223,509 |
| `https` | giong-http client and server over TLS (rustls), HTTP/1.1 | 30,681 | 31,775 |
| `web` | a giong-web router with a path parameter, raw client | 10,988 | 11,894 |
| `redis-get` | giong-redis `GET` against a fake server | 7,745 | 8,522 |
| `postgres-query` | giong-postgres query, one parameter, one row, fake server | 9,542 | 10,368 |
| `mysql-query` | giong-mysql query, one parameter, one row, fake server | 13,034 | 13,861 |

A TLS receive on an established connection stopped holding room for the handshake's sends,
and the stream writes of the core and of giong-tls stopped keeping their arguments twice
(`changelog.d/perf-h2-idle-memory.md`): `https` 31,605 to 30,681 on io_uring (−2.9%) and
32,685 to 31,775 on epoll (−2.8%), `h2-streams` −1.1%, `http1-client` and `tcp-echo` −0.4
to −0.6%, `h2` −0.4%, the database cases −0.2 to −0.3%, `http1` −0.2% and `web` −0.1 to
−0.2% ([`bench/results/h2-idle-memory/`](../results/h2-idle-memory/README.md)).

Each turn of a worker's event loop called helpers out of line that almost always find
nothing to do: the deferred-close list, the timers, the io_uring completion queue, and the
drop of the empty vector of finished resources the driver hands back. Their tests are now
inline and their work out of line ([#288](https://github.com/giong-run/Giong/issues/288)):
`yield` 481 to 358 on io_uring (−25.6%) and 359 to 306 on epoll (−14.6%), `spawn` −18.3%
and −9.5%, `channel` −14.6% and −7.3%, `tcp-echo` −4.1% and −3.6%, the database cases −2%
to −3.9%.

The io_uring driver's state changed shape with
[#322](https://github.com/giong-run/Giong/issues/322) (a bound on received data waiting on the
heap for readers that stopped reading), which moved the code the small cases run through:
`spawn` 1,376 to 1,346, `yield` 496 to 481, `channel` 1,721 to 1,690, `tcp-echo` 3,663 to 3,612
and `udp-echo` 4,165 to 4,110 (baseline rows). The new check runs only while data waits on the
heap; every other case moved by less than 1%.

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

#227 (a channel receive or send that is ready at once counts against the task's budget)
also stopped the bounded channel calling its grant and wake-up functions when no sender
waits: `channel` 1,756 to 1,721 on io_uring and 1,481 to 1,446 on epoll, measured on the
same machine before and after; the other cases did not move by more than 0.02%.

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

Four more cases measure memory, outside valgrind: `idle-http1`, `idle-h2`, `idle-https` and
`idle-h2-tls` open 2,000 idle connections to a giong-http server in the process (from a child process,
`bench-instr idle-client`, with blocking std sockets) and record how much the live heap and
the resident set grew per connection. The clients are in another process so that the server's
descriptors are numbered in the order it accepts them: the driver keeps a table indexed by
descriptor number, and with the clients on a thread of the same process the two ends took
numbers in scheduling order, so the table grew by different steps from run to run (2,166 or
2,211 heap bytes per `idle-http1` connection from the same build, 2026-11-22). For
`idle-https` the child does each TLS handshake and one keep-alive request with rustls, then
drops its TLS state and keeps the socket (the server cannot tell; nothing is sent);
`idle-h2-tls` does the same with ALPN `h2` and the HTTP/2 exchange of `idle-h2` inside TLS. Every count
is the server's alone, handshakes included. The child opens its connections 32 at a time, each
batch once the server has accepted the last (the parent waits with `yield_now`, which allocates
nothing). `idle-http1` sends no request, so before that nothing made the child wait for the
server, and how many connections the server took in one turn depended on the runner's speed;
the io_uring driver's completion queue and reap buffer grow to the largest such turn, and CI
read 1,262.3, 1,237.7 or 1,225.4 heap bytes per connection from the same tree, on aws and
azure kernels alike ([#507](https://github.com/giong-run/Giong/issues/507)). The heap count
is now exact: it repeats within a byte per connection, and within 0.04% across GitHub's runners
(Intel and AMD, two clouds; item 6 of [`conditions.txt`](../results/instr/conditions.txt)). Its
limit is 1%, and the baseline's idle rows are taken from a CI run, not from a development
machine. The resident set moves with the
allocator's choices (2.7% between rounds for `idle-h2`); its limit is 10%. For `idle-h2` the child sends the preface, SETTINGS and
one `GET /` on each connection and reads the answer, as `bench/h2.sh` does: until 0.9 it sent no
request, and missed that 0.8.0 kept each connection's memory for streams for a second after a
request (6,852 heap and 7,371 resident bytes per connection on 0.8.0 with the request, against
4,427 and 4,801 with the fix; 2026-11-16). HTTP/2 connections are opened one at a time, each
after the server's answer: opened all at once, the receive buffer pool runs low and the driver
moves bytes to the heap depending on timing, which made the heap per connection vary by 7%.

| case | heap per connection | resident set per connection |
|---|---:|---:|
| `idle-http1` (io_uring / epoll) | 1,217 / 1,329 bytes | 1,356 / 1,442 bytes |
| `idle-h2` (io_uring / epoll) | 2,518 / 2,535 bytes | 2,845 / 2,798 bytes |
| `idle-https` (io_uring / epoll) | 5,141 / 5,158 bytes | 5,550 / 5,458 bytes |
| `idle-h2-tls` (io_uring / epoll) | 6,397 / 6,414 bytes | 6,978 / 6,865 bytes |

The instruction counts above were measured on a 4-vCPU Xeon VM (Linux 6.18), last on 2026-11-27,
and are the same on CI's runners. The memory is from the CI run of the change that moved an
HTTP/2 connection to a task of its own and made the futures of TLS receives and of writes
smaller (2026-11-27), which gave the same heap bytes as a 4-vCPU VM: before it, 1,318, 3,318,
5,285 and 9,683 bytes on io_uring
([`bench/results/h2-idle-memory/`](../results/h2-idle-memory/README.md)); before the change
that made a connection's future keep across an `.await` only what waiting for the next
request needs, 2,062, 4,566 and 6,069 for the first three
([`bench/results/idle-conn-memory/`](../results/idle-conn-memory/README.md)); before the
change that moved rustls to its unbuffered API, which leaves an idle connection no receive
buffer, `idle-https` was 10,834 bytes
([`bench/results/tls-idle/`](../results/tls-idle/README.md)). What CI compares against is `baseline.csv`.

## When the check fails

The job prints a table of every metric, its baseline and its change, and stores the run's
counts and a ready `baseline.csv` as the run's `instr` artifact.

- **The change is meant to cost or save this much** (a feature on the hot path, an
  optimisation): take `baseline.csv` from the artifact, commit it as
  `bench/instr/baseline.csv` in the same pull request (with `bench/results/instr/raw.csv`
  from `scripts/instr.py update` if measured locally on a comparable machine), and say in the
  description what moved and why. A saving is checked as strictly as a cost, so that the
  baseline always describes `main`. Take the memory rows (`idle-*`) from the artifact in any
  case: a development machine's kernel gives other numbers than the runners agree on.
- **It passes, with rows listed above the table:** those moved by more than half their limit.
  If the pull request is not meant to move them, look before merging; a step that passes
  still stays in every later comparison.
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
