# giong-sim

A deterministic simulation for Gióng's tests: Gióng's real epoll driver runs
on a simulated kernel, everything that could vary is chosen by a seed, and
faults are injected on purpose. A failing run prints its seed, and
`GIONG_SIM_SEED=<seed>` replays exactly that run, step for step.

Not published and not part of Gióng's API. It changes nothing in the crate
itself: no feature flag, no hook, no cost on the real drivers.

```sh
scripts/check.sh sim          # what CI runs
cd sim && cargo test --release
```

## How it works

A test binary that links `giong-sim` defines the libc functions the runtime
calls (`socket`, `connect`, `send`, `recv`, `epoll_wait`, `eventfd`,
`clock_gettime`, `nanosleep`, `futex` through `syscall`, `getrandom`,
`pthread_create`, `openat`, `pread64` and more), and the dynamic linker
prefers the executable's symbols to libc's. Inside `Sim::run`:

- **Network**: TCP, UDP and Unix sockets between sockets of the process, in
  memory, with Linux's rules where tests depend on them (port sharing with
  `SO_REUSEADDR`/`SO_REUSEPORT`, data still readable after a reset, ICMP port
  unreachable on connected UDP, edge-triggered epoll).
- **Time**: virtual. It moves by a small cost per call, and jumps to the next
  timer when every thread waits, so an idle hour costs nothing.
- **Threads**: run one at a time. The seed decides who runs at each blocking
  call and where a thread is preempted, so several workers and the blocking
  threads are deterministic too.
- **Randomness**: `getrandom` draws from the seed.
- **Files**: real, but calls on files under a chosen prefix can be slowed
  down, fail with `EIO` or `ENOSPC`, or transfer less than asked.

io_uring calls fail with `ENOSYS` and `GIONG_DRIVER=epoll` is set, so the
runtime runs on its epoll driver. Each step goes into a trace whose hash
compares runs; `Report::first_difference` finds where two runs part.

## Faults

`Faults` sets each probability; the profiles:

| Profile | What it adds | A correct program |
|---|---|---|
| `none` | nothing | passes |
| `loopback` | jittery delivery, spurious `EAGAIN` on non-blocking calls, spurious wake-ups of `epoll_wait` and futex waits, preemption | passes: this is what a real loopback interface can show |
| `benign` | `loopback` plus split, lost (retransmitted) and delayed segments, lost SYNs (retried, at most three in a row), short reads and writes, duplicated and reordered datagrams | passes, with no error showing |
| `hostile` | `benign` plus resets, refused connects, aborted accepts, lost datagrams, the monotonic clock jumping forward (a stalled host) | reports errors; never hangs, panics, leaks or gives a wrong answer |

`Faults::disk(prefix, error, full, slow)` adds disk faults for files under
`prefix`.

## What runs

| Test | What it checks |
|---|---|
| `suite` | The core crate's own tests (`tests/*.rs` of the root crate), each in a child process: 180 pass, 17 are skipped with a reason (see `build.rs`). `GIONG_SIM_FAULTS=loopback` runs them under faults. |
| `basics`, `workers` | Virtual time, turn taking, the same seed giving the same trace, a seed replaying the same in a fresh process. |
| `hostile` | A TCP echo server and its clients under 100 hostile seeds; benign faults never show as errors; a partition makes connects time out. |
| `http` | giong-http over plain TCP, TLS and HTTP/2: a response never goes to the wrong request, benign faults never show as errors. |
| `redis`, `postgres` | giong-redis and giong-postgres pools against small fake servers: never a wrong answer. |
| `disk` | `giong::fs` under slow disks, `EIO`, `ENOSPC` and short transfers: data written and synced reads back unchanged or an error is reported. |
| `soak` | Months of uptime in simulated time: an HTTP server and its client with an hourly burst of requests, quiet hours between, and timers that span days. Nothing grows per request (descriptors, tasks, timers, operations), timers are never early and do not drift. 90 days under benign faults and 10 runs of 12 days under hostile faults, about 210 simulated days, take about 3 s. Every night `scripts/check.sh sim-long` runs the hostile soak for 1,200 new seeds of 521 days each (about 1,700 simulated years). |

Every run of `sweep` fails if it leaves a descriptor open (`Report::open`).

## When a run fails

The report names the seed, the threads and the last steps of the trace:

```text
giong-sim: run failed with seed 14 at virtual time 0.211427 s after 59 steps
  deadlock: every thread waits and no event or timeout is pending
  replay: GIONG_SIM_SEED=14
  threads:
    t0: Blocked(AllExited, None)
    t1: Blocked(Epoll { ep: 2 }, None)
  last steps:
         2441583 t1 epoll_wait o2 -> []
         ...
```

Replay it with the same test and `GIONG_SIM_SEED=14`. More context:
`GIONG_SIM_TAIL=<n>` shows the last n steps, `GIONG_SIM_DUMP=<file>` writes
the whole trace of a run started with `full_trace(true)`,
`GIONG_SIM_VERBOSE=1` prints each suite test's steps and virtual time.
`GIONG_SIM_SEEDS=<n>` sets how many seeds the sweeps run,
`GIONG_SIM_SEED_FROM=<n>` the first seed of the hostile soak, and
`GIONG_SIM_SOAK_DAYS=<n>` how long the soak runs. A seed found by the nightly
long runs replays with `GIONG_SIM_SEED=<seed> GIONG_SIM_SOAK_DAYS=3650`.

## What it does not cover

- **io_uring.** Only the epoll driver runs here. The io_uring driver, the
  default on modern kernels, is covered by the ordinary tests, the old-kernel
  tiers and the QEMU kernels in CI, not by this.
- **The real kernel and the NIC.** The network is a model of Linux's
  behaviour, not Linux: its buffers, congestion control, offloads and drivers
  are not here. The 72-hour soak on bare metal stays the check for those.
- **Signals, control messages, FIFOs and pipes, and real blocking calls.**
  The tests that need them are skipped in `suite`, each with its reason.
- **The wall clock.** `CLOCK_REALTIME` starts at 2030-01-01 in every run.
- **Memory pressure.** Inside a run `/proc/meminfo`, `/proc/self/cgroup`
  and `/sys/fs/cgroup` do not exist, so giong-http's low-memory refusal
  (`Server::min_free_memory`) never refuses: what the host has free must not
  decide a simulated run. The refusal has its own tests and CI job.
- **Process-wide state.** Something set up on first use in a process (a
  `OnceLock` that reads the clock) makes its calls in the first run only; a
  sweep therefore starts with one run whose report it drops, so that every
  reported run, and a replay in a fresh process, starts from the same state.

The core crate's tests that fail under `benign` faults do so because they
assume loopback: one read returns a whole small write, datagrams arrive in
order and once. That holds on a real loopback interface and is why the suite
runs under `loopback`.

## Traps for whoever changes it

- Calls on real descriptors (a file the first panic opens to read debug
  info, `/proc`) must take no step and draw nothing from the seed, or a cold
  process and a warm one diverge.
- Each run executes on a fresh thread: the standard library seeds its
  `HashMap` keys per thread, and those draws would otherwise land in one run
  only.
- A spurious `EAGAIN` must come with a new readiness edge: with
  edge-triggered epoll, real Linux never leaves queued data without one, and
  a runtime would wait forever.
- Nothing may grow per step or per connection: a run of decades takes
  billions of steps and millions of connections. Per-step hashes are kept
  only with `full_trace`, and a connection is dropped once both of its
  sockets have closed. The soak's memory stays at about 5 MB whatever its
  length.
- A deadlock aborts the process after its report, so `suite` runs each test
  in a child process.
- Internal calls use raw `syscall` instructions, never libc, so the
  simulation never calls itself.
