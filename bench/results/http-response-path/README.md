# The response path of giong-http: fewer instructions, no allocation per JSON response

giong-http and giong before and after the change of `changelog.d/perf-http-response-path.md`.
On the bare-metal run of 2026-11-27 (`bench/results/bare-metal-epyc-4345p/linux-7.0/`),
giong-http on one worker answered `/json` at 464k requests/s against 527k for `bench/tokio-raw`,
and took 2.15 µs of CPU per request against 1.89. `bench/giong`, the runtime with a hand-rolled
HTTP loop, did 559k: the difference is in giong-http's own work. Callgrind on `bench/instr`'s
`http1` case put about 1,300 of its 8,160 instructions per request in timers and clock reads,
260 in the shutdown signal around every wait, 750 in building the response head, and one
allocation per response. Four parts:

- **A constant `Content-Type` stays borrowed.** `Response::json` and `Response::text` went
  through `Response::header`: the name and value were checked, compared with the four headers
  the server manages, trimmed, and copied into a new buffer, one allocation per response. The
  header lines are now a `Cow<'static, [u8]>` (the same 24 bytes as the `Vec<u8>` it replaces),
  borrowed while they are one constant line and copied into an owned buffer only when another
  header is added.
- **A `200 OK` status line is one copy** instead of a prefix, the code formatted digit by digit,
  a space and the reason.
- **The `Date` header is read from the worker's clock of the turn.** It read the wall clock for
  every response. The worker already reads its monotonic clock once per turn of its event loop
  for its timers; the formatted date is now kept until that clock reaches the next second (a
  hidden `giong::time::turn_clock`, for the crates of the workspace). The text still changes with
  the second; within a turn that crosses it, it can stay the earlier one for as long as the turn
  has run, as nginx's cached time does.
- **A `ShutdownSignal` holds no reference to the runtime.** giong-http puts `until_shutdown`
  around every wait of a keep-alive connection; making the signal cloned an `Arc` shared by all
  the workers, and dropping it gave it back: two atomic operations on one cache line per
  request, on every worker. The signal now reaches the runtime through the worker it is polled
  on, and is 24 bytes instead of 32.

Machine: 4-vCPU cloud VM (Intel Xeon), Linux 6.18, rustc 1.97.0, valgrind 3.22, 2026-11-27.
"main" is `main` at 6c2c852; "change" is the change on top of it.

## Instructions and allocations (bench/instr)

`scripts/instr.py measure --rounds 3` on both trees: [`instr-main.txt`](instr-main.txt),
[`instr-change.txt`](instr-change.txt) (medians), [`instr-change-raw.csv`](instr-change-raw.csv)
(every round of the change, from `scripts/instr.py update`). The HTTP cases count the client
and the server; the client of `http1` and `web` is a raw socket, so those rows are mostly the
server.

| case | metric | main (io_uring / epoll) | change (io_uring / epoll) | change |
|---|---|---:|---:|---:|
| `http1` | instructions per request | 8,168.6 / 9,073.5 | **7,438.5 / 8,349.5** | **−8.94% / −7.98%** |
| `web` | instructions per request | 10,989.5 / 11,900.5 | **10,320.7 / 11,231.5** | **−6.09% / −5.62%** |
| `http1-client` | instructions per request | 19,860.3 / 20,955.6 | **19,136.3 / 20,230.8** | **−3.65% / −3.46%** |
| `h2-streams` | instructions per ten requests | 222,169.6 / 223,541.4 | **217,014.3 / 218,351.5** | **−2.32% / −2.32%** |
| `https` | instructions per request | 30,693.9 / 31,776.0 | **30,016.0 / 31,103.7** | **−2.21% / −2.12%** |
| `h2` | instructions per request | 28,063.0 / 29,193.7 | **27,550.9 / 28,677.4** | **−1.82% / −1.77%** |
| `http1`, `web`, `http1-client`, `h2`, `https` | allocations per request | 1, 3, 7, 14, 13 | **0, 2, 6, 13, 12** | one fewer |
| `h2-streams` | allocations per ten requests | 150 | **140** | ten fewer |
| `idle-h2` | heap bytes per connection | 2,517.9 / 2,534.6 | **2,509.9 / 2,526.6** | −8 bytes |
| `idle-h2-tls` | heap bytes per connection | 6,396.8 / 6,413.5 | **6,388.8 / 6,405.5** | −8 bytes |

The idle cases answer one request on each connection before they count, so their allocation
rows went down by one too (`idle-h2` 25.1 to 24.1, `idle-https` 122.2 to 121.2, `idle-h2-tls`
142.2 to 141.2). `idle-http1` and `idle-https` keep the same heap bytes: their tasks did not
change size class. Every other row (`spawn`, `yield`, `oneshot`, `channel`, `timeout`,
`tcp-echo`, `udp-echo`, `file-read`, the three database clients) is the same to the decimal,
except epoll `redis-get`, 0.07% lower. No row is higher.

## The server alone (bench/giong-http, callgrind)

`bench/results/idle-conn-memory/callgrind.sh` on both builds of `bench/giong-http`, two runs of
each, interleaved ([`callgrind.txt`](callgrind.txt)): the server's instructions per request on
a keep-alive connection, 2,000 requests after 300 of warm-up.

| path | main | change | change |
|---|---:|---:|---:|
| `/json` | 6,387.1 | **5,566.1** | **−12.9%** |
| `/users` (8.3 KB of JSON written by the handler, 100 floats formatted) | 179,124.0 | **178,319.1** | −0.45% |

## Time (bench/giong-http, wrk, one worker)

The server on CPU 0, `wrk -t3 -c64` on CPUs 1 to 3 of the same VM, `/json`, 8 s after 2 s of
warm-up, three rounds of each build, interleaved ([`vm-wrk.txt`](vm-wrk.txt)); CPU time from
`/proc/<pid>/stat`. Over loopback on a VM the kernel takes 85% of the server's time and moves
from round to round, so this only confirms the direction; the bare-metal run is the measure.

| | main | change |
|---|---:|---:|
| user CPU per request (median) | 1.013 µs | **0.839 µs (−17%)** |
| requests/s (median) | 149,854 | 153,330 (+2.3%) |
