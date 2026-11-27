# One alarm per connection: giong-http's timeouts and shutdown signal, armed once

giong-http before and after the change of `changelog.d/perf-http-conn-alarm.md`, on top of the
response-path change (`bench/results/http-response-path/`). Callgrind on `bench/instr`'s `http1`
case after that change still put about 780 of the server's instructions per request in the wait
for the next request: `until_shutdown(timeout(idle, recv))` armed a timer in the wheel (with a
read of the clock) and cancelled it, and registered and removed a shutdown waker (a clone and a
drop of the task's waker, two atomic operations), on every request. Two parts:

- **One alarm per connection.** A hidden `giong::time::Alarm` (16 bytes, for the crates of the
  workspace) is one entry in the worker's timer wheel that stays armed from one wait to the
  next. A later deadline than the one it keeps only changes the deadline in its entry: the
  entry stays in its slot, and when the wheel reaches that slot it moves the entry to the slot
  of its new deadline, as it does for a timer cascading from a higher level. An earlier one
  moves it at once. The worker wakes armed alarms at a shutdown, as it wakes the tasks waiting
  for the shutdown signal. The connection polls what it receives first and the alarm only when
  nothing is there, so a request costs one look at the alarm. An alarm left on an older
  deadline while requests come back to back wakes its connection once, in vain, when that
  deadline passes. The shutdown's linger for a request already on its way is the same alarm
  moved to a new deadline, inside the same wait, so the connection's future has no second
  waiting state (a second one kept the result of the first across it and made every state of
  the connection 96 bytes larger).
- **The server's settings are shared by the connections of a worker.** Every connection kept
  its own copy of them (120 bytes); it now keeps an `Rc` of one copy per worker and server.

Machine: 4-vCPU cloud VM (Intel Xeon), Linux 6.18, rustc 1.97.0, valgrind 3.22, 2026-11-27.
"before" is the response-path change (`perf/http-one-worker`, 78b15cc); "change" is this change on
top of it.

## Instructions and allocations (bench/instr)

`scripts/instr.py check --rounds 3` on both trees: [`instr-before.txt`](instr-before.txt) and
[`instr-change.txt`](instr-change.txt) (the "now" column of each is that tree's medians, against
the same baseline), [`instr-change-raw.csv`](instr-change-raw.csv) (every round of the change).
The HTTP cases count the client and the server; the client of `http1` and `web` is a raw
socket, so those rows are mostly the server.

| case | metric | before (io_uring / epoll) | change (io_uring / epoll) | change |
|---|---|---:|---:|---:|
| `http1` | instructions per request | 7,438.7 / 8,347.5 | **6,971.1 / 7,882.6** | **−6.29% / −5.57%** |
| `web` | instructions per request | 10,326.3 / 11,233.4 | **9,858.1 / 10,771.6** | **−4.53% / −4.11%** |
| `http1-client` | instructions per request | 19,134.3 / 20,225.3 | **18,669.5 / 19,766.6** | **−2.43% / −2.27%** |
| `https` | instructions per request | 30,011.8 / 31,098.9 | **29,535.4 / 30,624.1** | **−1.59% / −1.53%** |
| `redis-get` | instructions per request | 7,745.1 / 8,521.8 | **7,723.1 / 8,499.8** | −0.28% / −0.26% |
| `idle-http1` | heap bytes per connection | 1,262.3 / 1,328.9 | **1,149.9 / 1,216.6** | **−112 bytes** |
| `idle-h2` | heap bytes per connection | 2,509.9 / 2,526.6 | **2,397.9 / 2,414.6** | **−112 bytes** |
| `idle-https` | heap bytes per connection | 5,141.3 / 5,157.9 | **5,028.9 / 5,045.6** | **−112 bytes** |
| `idle-h2-tls` | heap bytes per connection | 6,388.8 / 6,405.5 | **6,276.8 / 6,293.5** | **−112 bytes** |

Allocations are the same in every case. `h2` and `h2-streams` are within 0.02% either way: those
cases format a `Date` header that HPACK encodes, and the encoded bytes change with the second,
which moves them by about as much from run to run of the same build (55.103M and 55.108M
instructions in two runs of `before`'s `h2` under callgrind, 55.106M and 55.119M of the
change's). Every other row is the same to the decimal. No row is higher.

Two traps on the way, both avoided in the numbers above: splitting `Timers::insert` for the
alarm made LLVM stop inlining the wheel's `link` into it, which made every timer armed dearer
(`h2` +72 instructions per request, +0.2%) until `link` was marked `#[inline(always)]`; and
polling the alarm before the socket looked at it twice per request instead of once, which with
a second read of the clock for a deadline within a millisecond (dropped: a connection's
deadlines are seconds away) cost 300 instructions per request.

Sizes of the HTTP/1.1 connection's futures ([`sizes.txt`](sizes.txt), `-Zprint-type-sizes`): the
connection 336 to 240 bytes, its task's future 856 to 760.

## The server alone (bench/giong-http, callgrind)

`bench/results/idle-conn-memory/callgrind.sh` on both builds of `bench/giong-http`, two runs of
each, interleaved ([`callgrind.txt`](callgrind.txt)): the server's instructions per request on
a keep-alive connection, 2,000 requests after 300 of warm-up.

| path | before | change | change |
|---|---:|---:|---:|
| `/json` | 5,565.8 | **5,123.0** | **−7.96%** |
| `/users` (8.3 KB of JSON written by the handler, 100 floats formatted) | 178,319.2 | **177,891.7** | −0.24% |

Against `main` before the response-path change (6,387.1 for `/json`), the two changes together
take 19.8% of the server's instructions per request.

## Time (bench/giong-http, wrk, one worker)

The server on CPU 0, `wrk -t3 -c64` on CPUs 1 to 3 of the same VM, `/json`, 8 s after 2 s of
warm-up, three rounds of each build, interleaved ([`vm-wrk.txt`](vm-wrk.txt)); CPU time from
`/proc/<pid>/stat`. Over loopback on a VM the kernel takes 85% of the server's time and moves
from round to round, so this only confirms the direction.

| | before | change |
|---|---:|---:|
| user CPU per request (median) | 0.876 µs | **0.777 µs (−11%)** |
| requests/s (median) | 150,746 | 152,460 (+1.1%) |
