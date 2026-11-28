# giong-http on one worker: the write timeout on the connection's alarm, ASCII method and target

giong-http before and after the change of `changelog.d/perf-http-write-alarm.md`. The 1.5.x
bare-metal run left giong-http on one worker 5.7% behind Tokio's hand-written loop
(`bench/tokio-raw`) in requests per second, after #499. `bench/instr`'s `http1` case sends one
request at a time, so every response write completes at its first poll; under load a busy
worker batches its sends into io_uring submissions, and then every response write waits once.
Callgrind on bench/giong-http's `/json` with 32 connections in a closed loop showed what that
wait cost per request: the write timeout (`until_stalled`) armed a `Sleep` in the worker's
timer wheel and removed it again (`Timers::insert_entry`, `Sleep::poll`, `Sleep::unregister`,
a read of the clock), about 400 instructions. Two parts:

- **The write timeout waits on the connection's alarm.** Since the per-connection alarm
  change (`bench/results/http-conn-alarm/`) a connection's reads wait on one hidden
  `giong::time::Alarm` that stays armed. The write timeout now uses the same alarm: a write
  that waits moves the alarm's deadline (an earlier deadline than the one it keeps relinks its
  entry; a later one only changes the deadline it keeps), and reads the worker's clock of the
  turn, which the alarm compares with anyway. A shutdown wakes the alarm, which the write
  counts as progress: a write goes on through a shutdown, as before. `UntilStalled` keeps a
  `&mut Alarm` and an `Option<Instant>` instead of an `Option<Sleep>`.
- **`Request::method` and `Request::target` check ASCII.** The parser only accepts ASCII in
  both, and both are read for every request (by the server, and again by a handler that
  routes): an ASCII check, then `from_utf8_unchecked`, is cheaper than `from_utf8`. Non-ASCII
  bytes still go through `from_utf8`, so the `unsafe` block's only premise is the check just
  before it.

Machine: 4-vCPU cloud VM (Intel Xeon @ 2.10 GHz), Linux 6.18, rustc 1.97.0, valgrind 3.22,
2026-11-28. "base" is giong-http on main (6e9c2f5), "alarm" the first part, "both" both parts.

## Instructions per request under load

[`callgrind-loaded.sh`](callgrind-loaded.sh): bench/giong-http with `GIONG_WORKERS=1` under
callgrind, `lab-loadgen` with 32 connections on two threads, counters zeroed and dumped
around a 4 s and a 12 s window; instructions per request = the difference of the two dumps
over the difference in requests. Raw: [`callgrind-loaded.csv`](callgrind-loaded.csv).

| build | runs | mean |
|---|---|---:|
| base | 5,525 / 5,660 | 5,593 |
| alarm | 5,530 / 5,391 | 5,460 (−2.4%) |
| both | 5,388 / 5,375 | **5,381 (−3.8%)** |

Self cost per request in the 12 s windows ([`self-cost-base-1.txt`](self-cost-base-1.txt),
[`self-cost-alarm-1.txt`](self-cost-alarm-1.txt), [`self-cost-both-1.txt`](self-cost-both-1.txt);
two runs each, means):

| part | base | alarm | both |
|---|---:|---:|---:|
| timers and clocks (`giong::time`, `Timespec`, `clock_gettime`, `turn_clock`) | 753 | 582 | **578** |
| `from_utf8` / `head_str` | 140 | 140 | **88** |

The alarm's own poll costs more than before (it now moves twice a request: to the write's
deadline, then back to the idle one), but less than the timer it replaces.

## Losses, stated plainly

- **This does not close the one-worker gap on its own.** Under the same load and method,
  Tokio's hand-written loop (`bench/tokio-raw`) runs 2,533 instructions per request
  (2,513 / 2,553, rows `tokio-raw` in the CSV) against giong-http's 5,381. giong-http does
  more per request (routing, header checks, the `Date` header, timeouts, per-IP limits, the
  shutdown signal); on bare metal most of each request's time is likely the kernel's, which
  would be why twice the userspace instructions came to a 5.7% gap in requests per second
  (not measured: no profile of that run). What the 3.8% buys there needs the next bare-metal run.
- The 30 s write timeout is earlier than the 60 s idle timeout, so every write that waits
  relinks the alarm's entry in the wheel; it is still cheaper than a timer armed and
  removed.
