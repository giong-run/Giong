# The HTTP client's cost per request

giong-http's client before and after the changes of `changelog.d/perf-http-client-cost.md`:
HTTP/2 request fields borrowed from the request instead of copied, a standard method not
copied, the request's description made only when it times out, the URL's host shared with
the origin its connection is looked up by (`Rc<str>`), one allocation for a response's head
and body over HTTP/2, a per-thread waker for checking idle connections, URLs parsed with
byte searches, and each well-formed header line of a response read in one pass (the careful
reader, with its messages, still reads every other line).

Machine: 4-vCPU cloud VM (Intel Xeon 2.1 GHz), Linux 6.18, rustc 1.97.0, valgrind 3.22,
2026-11-26. "main" is `main` at 1cb2988; "change" is the change on top of it.

## Instructions and allocations (bench/instr)

`scripts/instr.py measure --cases http1-client,h2,h2-streams,https --rounds 3` on both trees.
Each case runs the client and the server in one process, so the counts are both together;
`h2-streams` is ten requests. [`instr-main-1cb2988.csv`](instr-main-1cb2988.csv),
[`instr-change.csv`](instr-change.csv).

| case | driver | main instructions | change | | main allocations | change |
|---|---|---:|---:|---:|---:|---:|
| `http1-client` | io_uring | 21,845 | 20,088 | −8.0% | 11 | 7 |
| `http1-client` | epoll | 22,941 | 21,184 | −7.7% | 11 | 7 |
| `h2` | io_uring | 29,720 | 28,241 | −5.0% | 22 | 14 |
| `h2` | epoll | 30,852 | 29,374 | −4.8% | 22 | 14 |
| `h2-streams` | io_uring | 240,259 | 225,085 | −6.3% | 230 | 150 |
| `h2-streams` | epoll | 241,640 | 226,198 | −6.4% | 230 | 150 |
| `https` | io_uring | 33,883 | 32,114 | −5.2% | 17 | 13 |
| `https` | epoll | 34,953 | 33,184 | −5.1% | 17 | 13 |

The server's code did not change (`http1`, the server alone, is 8,289 instructions on
io_uring and 9,200 on epoll in both trees, and `web` 11,268 and 12,174), so the client's
own instructions went down by 13% over HTTP/1.1 (from about 13,550 to 11,800 a request). Where the allocations went: 4 a request
over HTTP/1.1 (the request's description made a second time, the method, the origin's copy
of the host, the waker of the idle check), 8 over HTTP/2 (the same but the waker, the four
strings of the `user-agent` and `accept-encoding` fields, and the reallocation of a
response growing from its head to its body).

## Requests per second and the client's CPU (bench/http-client-ab.sh)

`A=<main tree> B=<change tree> bench/http-client-ab.sh 8 5`: one server (main's
`bench/giong-http`, three cores), and the two builds of `bench/http-client-giong` on the
fourth core, one after the other in every round, 64 tasks and at most 16 connections, as in
`bench/http-client.sh`. Medians of 8 rounds; `user` is the client's user CPU per request,
`CPU` user and system together. [`ab/raw.csv`](ab/raw.csv), [`ab/summary.txt`](ab/summary.txt).

| case | main req/s | change req/s | | user µs/req | | CPU µs/req | |
|---|---:|---:|---:|---:|---:|---:|---:|
| `http` json | 91,862 | 92,962 | +1.2% | 2.33 → 2.19 | −5.9% | 10.78 → 10.64 | −1.3% |
| `http` users | 82,726 | 85,174 | +3.0% | 3.39 → 3.06 | −9.9% | 11.96 → 11.61 | −2.9% |
| `https` json | 85,832 | 87,701 | +2.2% | 2.99 → 2.78 | −7.2% | 11.55 → 11.33 | −1.9% |
| `https` users | 67,468 | 67,150 | −0.5% | 5.93 → 6.04 | +1.8% | 14.71 → 14.73 | +0.1% |
| `h2` json | 281,060 | 289,371 | +3.0% | 1.92 → 1.75 | −8.8% | 2.17 → 2.06 | −5.3% |
| `h2` users | 60,100 | 61,244 | +1.9% | 6.68 → 6.28 | −5.9% | 9.87 → 9.45 | −4.3% |
| `h2c` json | 364,794 | 404,986 | +11.0% | 1.89 → 1.62 | −14.3% | 2.31 → 2.02 | −12.3% |
| `h2c` users | 68,876 | 70,142 | +1.8% | 4.67 → 4.10 | −12.1% | 6.95 → 6.58 | −5.3% |

`https` users was the one row on the wrong side, by 0.5%, with rounds of both builds
spread over 63,700 to 75,000. Twelve more rounds of the two `https` cases alone
([`ab-https/`](ab-https/summary.txt)) gave `https` users +2.5% (CPU per request −2.1%) and
`https` json +0.1% (user CPU −6.6%, CPU +0.4%). Over HTTP/1.1 most of the client's core is
the kernel's (8.3 of 10.8 µs a request on `http` json), so 13% fewer instructions of its
own show as 1% to 3% more requests; over HTTP/2 the client's time is mostly its own, and the
gain shows (h2c json +11%).

## A first run, not interleaved

Before `bench/http-client-ab.sh`, `bench/http-client.sh 2 5` ran three times in each tree in
turn (main, change, main, ...), 6 rounds of each build. [`first-run/raw.csv`](first-run/raw.csv).

| case | main req/s | change req/s | | client core busy (%) |
|---|---:|---:|---:|---|
| `http` json | 92,621 | 91,478 | −1.2% | 99 / 99 |
| `http` users | 82,796 | 88,182 | +6.5% | 98 / 99 |
| `https` json | 84,832 | 85,684 | +1.0% | 99 / 99 |
| `https` users | 65,882 | 68,380 | +3.8% | 99 / 99 |
| `h2` json | 283,245 | 294,194 | +3.9% | 61 / 60 |
| `h2` users | 64,346 | 61,152 | −5.0% | 59 / 57 |
| `h2c` json | 376,514 | 411,044 | +9.2% | 86 / 84 |
| `h2c` users | 71,678 | 69,331 | −3.3% | 47 / 44 |

The two `users` rows over HTTP/2 that went down are runs where the client's core was not
the limit (44% to 59% busy, the server's single worker for the one connection was), and
each build's rounds spread over 9,000 requests a second; interleaved, the same rows went up
by 1.9% and 1.8% with 4% to 5% less CPU per request.
