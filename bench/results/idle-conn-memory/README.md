# Idle connection memory: what a connection's future keeps while it waits

giong-http before and after the change of `changelog.d/perf-idle-conn-memory.md`. A
connection's future is allocated once, when the connection is accepted, and keeps its size
for as long as the connection lives, idle or not. rustc gives every local that lives across
more than one `.await` its own room in every state of that future, waiting for the next
request included, and an `async fn` keeps its arguments twice (as received, and moved into
its body). Before the change the request being answered (a value of about 200 bytes), the
response being written, the receipt of a body still on its way (with the decoder of a
chunked body) and the write of a file or large body all had their room in the connection's
future beside the state that waits for the next request. After it, each lives in a state of
its own or in a block on the heap that the worker reuses (`HeapFuture`): the rarer, larger
waits (a body still arriving, a file or a body of 32 KiB or more sent from where it is, the
HTTP/2 preface and the last close of an HTTP/2 connection) are allocated only while they
run, and `serve_h2` and `h2::serve` borrow their configuration instead of taking it by
value. Taking and giving back such a block got cheaper at the same time, which every
request gains from, since a handler's future lives in one: the future is made in its block
(`HeapFuture::with`) instead of made on the stack and copied there whole, the worker's cache
of blocks is reached without a borrow flag, and what is rare (a new size, a list that has to
grow) is kept out of the common path.

Machine: 4-vCPU cloud VM (Intel Xeon), Linux 6.18, rustc 1.97.0, valgrind 3.22, 2026-11-26.
"main-81588a2" is `main` at 81588a2; "change" is the change on top of it.

## Size of a connection's future

`-Zprint-type-sizes` of `bench/instr`, release build, for the handler of `bench/instr`'s
`http1` case; the heap block is what a connection costs from the moment it is accepted.

| future | main-81588a2 | change |
|---|---:|---:|
| HTTP/1.1 connection's task (heap block) | 1,752 bytes | **1,008 bytes** |
| HTTPS connection's task (heap block) | 3,480 bytes | **2,696 bytes** |
| `h2::serve` over TCP (the HTTP/2 connection's loop) | 2,120 bytes | **1,616 bytes** |
| `serve_h2` over TLS (HTTP/2 with TLS, on the heap) | 6,384 bytes | **4,112 bytes** |
| `Conn::run` (the HTTP/1.1 request loop) | 1,304 bytes | 560 bytes |
| `Conn::next_request` | 824 bytes | 448 bytes |

The heap per idle connection below moved by exactly these differences: 744 bytes for HTTP/1.1
(1,752 − 1,008), 1,248 for HTTP/2 (744 + 504) and 784 for HTTPS (3,480 − 2,696).

## Heap per idle connection and instructions per request (bench/instr)

`scripts/instr.py measure --rounds 3` on both trees.
[`instr-main-81588a2.csv`](instr-main-81588a2.csv), [`instr-change.csv`](instr-change.csv).

| case | metric | main-81588a2 | change | change |
|---|---|---:|---:|---:|
| `idle-http1` io_uring | heap bytes per connection | 2,062.3 | **1,318.3** | **−36.1%** |
| `idle-http1` epoll | heap bytes per connection | 2,128.9 | **1,384.9** | **−34.9%** |
| `idle-h2` io_uring | heap bytes per connection | 4,565.9 | **3,317.9** | **−27.3%** |
| `idle-h2` epoll | heap bytes per connection | 4,582.5 | **3,334.5** | **−27.2%** |
| `idle-https` io_uring | heap bytes per connection | 6,069.3 | **5,285.3** | **−12.9%** |
| `idle-https` epoll | heap bytes per connection | 6,085.9 | **5,301.9** | **−12.9%** |
| `idle-http1` io_uring / epoll | resident bytes per connection | 2,187.3 / 2,242.6 | 1,464.3 / 1,507.3 | −33.1% / −32.8% |
| `idle-h2` io_uring / epoll | resident bytes per connection | 4,907.0 / 4,862.0 | 3,702.8 / 3,629.1 | −24.5% / −25.4% |
| `idle-https` io_uring / epoll | resident bytes per connection | 6,479.9 / 6,381.6 | 5,695.5 / 5,599.2 | −12.1% / −12.3% |
| `http1` io_uring / epoll | instructions per request | 8,293.7 / 9,204.5 | **8,172.6 / 9,089.5** | **−1.46% / −1.25%** |
| `http1-client` io_uring / epoll | instructions per request | 21,851.3 / 22,946.6 | 21,730.3 / 22,825.6 | −0.55% / −0.53% |
| `https` io_uring / epoll | instructions per request | 33,500.1 / 34,580.0 | 33,364.2 / 34,455.8 | −0.41% / −0.36% |
| `web` io_uring / epoll | instructions per request | 11,263.4 / 12,174.5 | **10,999.5 / 11,916.5** | **−2.34% / −2.12%** |
| `h2` io_uring / epoll | instructions per request | 29,743.6 / 30,875.0 | 29,667.8 / 30,797.9 | −0.25% / −0.25% |
| `h2-streams` io_uring / epoll | instructions per ten requests | 240,483.6 / 241,812.2 | 240,048.0 / 241,304.4 | −0.18% / −0.21% |

Allocations are unchanged in every case: the blocks the worker reuses for `HeapFuture` are
not new allocations once warm. The three rounds of a case lie within 0.07% of their median
(within 0.05% for the HTTP/2 cases), so the smallest of these changes (`h2-streams`, −0.18%)
is outside the noise of the measurement.
The cases `idle-https`, `https`, `h2`, `h2-streams` and `idle-h2` of the change were measured
again, in a second run with the same settings, after the last edit to the HTTPS
connection's task; the csv has that run for them.

## Instructions per response (bench/giong-http, callgrind)

[`callgrind.sh`](callgrind.sh) on both builds of `bench/giong-http` (one worker, plain TCP,
2,000 keep-alive requests after 300 of warm-up, two runs of each, interleaved;
[`callgrind.txt`](callgrind.txt)). `bytes64k` answers with a 64 KiB body and `file64k` with
64 KiB of a file, the two responses whose write moved to a heap block.

| path | main-81588a2 | change | change |
|---|---:|---:|---:|
| `/json` | 6,588.8 | **6,404.5** | **−2.8%** |
| `/file?n=65536` (`file64k`) | 5,993.8 | **5,808.0** | **−3.1%** |
| `/bytes?n=65536` (`bytes64k`) | 6,021.2 | **5,837.0** | **−3.1%** |

The two runs of each build gave the same count to within 3 instructions.

## Resident memory, 8,000 idle HTTPS connections (bench/tls-idle.sh)

`bench/tls-idle.sh 3 8000`, and `PROTOCOL=h2 bench/tls-idle.sh 3 8000` for HTTP/2 over TLS
(`BINS` naming the two builds of `bench/giong-http`): one worker on CPU 3, an OpenSSL client
(`tls-idle/hold.py`) on the others opening the connections one by one, each with a full TLS
1.3 handshake and one keep-alive `GET /json` (for `h2`, the preface, SETTINGS and their
acknowledgement and the request on stream 1), then idle.
[`tls-http1/`](tls-http1/summary.txt), [`tls-h2/`](tls-h2/summary.txt).

| protocol | | anonymous memory per connection | resident set per connection | resident set with 8,000 |
|---|---|---:|---:|---:|
| HTTP/1.1 | main-81588a2 | 6.17 KiB | 6.19 KiB | 52.6 MiB |
| HTTP/1.1 | change | **5.40 KiB** | **5.43 KiB** | **46.7 MiB** |
| HTTP/2 | main-81588a2 | 12.80 KiB | 12.81 KiB | 104.4 MiB |
| HTTP/2 | change | **9.81 KiB** | **9.85 KiB** | **81.2 MiB** |

All three rounds of each gave the same numbers to the hundredth (the resident set within
0.2 MiB). An idle HTTP/2 connection over TLS still costs about twice an HTTP/1.1 one: its
session (HPACK tables, stream bookkeeping) and the TLS stream's receive and write futures
the HTTP/2 loop keeps are the next things to look at.

## Plain HTTP: 10,000 connections, four workers (bench/memory.sh)

`SERVERS="giong-http giong-http-epoll" bench/memory.sh 3` in each tree: 10,000 connections
from `bench/hold`, the server's resident memory with them held minus before, per connection,
median of three rounds, a fresh server for each scenario ("open": connected, nothing sent;
"idle": one keep-alive request answered; "pending": a request whose handler waits).
[`memory-main-81588a2/`](memory-main-81588a2/summary.txt),
[`memory-change/`](memory-change/summary.txt).

| server | open | idle | pending | peak under load |
|---|---:|---:|---:|---:|
| main-81588a2, io_uring | 2.2 KiB | 2.1 KiB | 6.8 KiB | 14.1 MiB |
| change, io_uring | **1.4 KiB** | **1.4 KiB** | **6.1 KiB** | 14.1 MiB |
| main-81588a2, epoll | 2.3 KiB | 2.4 KiB | 6.8 KiB | 15.0 MiB |
| change, epoll | **1.6 KiB** | **1.7 KiB** | **6.1 KiB** | 15.0 MiB |

Every round of each gave the same value to within 0.05 KiB. The requests per second `wrk`
reached in the load scenario are in `raw.csv` but say nothing here: other builds ran on the
same machine during both runs (the script itself calls them only indicative). Throughput
is measured below, alone.

## HTTP/2: 5,000 connections, one worker (bench/h2.sh)

`PARTS=memory MEM_SERVERS=giong-http bench/h2.sh 3` in each tree (the options are new;
`bench/h2.sh` without them runs as before): 5,000 connections over HTTP/2 with prior
knowledge from `bench/hold`, one worker. "Idle": one request answered on each connection;
"pending": one request still being handled; "per stream": each further request in progress
on a connection (ten in all). [`h2-memory-main-81588a2/`](h2-memory-main-81588a2/memory.csv),
[`h2-memory-change/`](h2-memory-change/memory.csv).

| | idle | pending | per stream |
|---|---:|---:|---:|
| main-81588a2 | 4.68 KiB | 6.07 KiB | 1.51 KiB |
| change | **3.47 KiB** | **4.86 KiB** | 1.50 KiB |

All three rounds gave the same values to within 0.01 KiB. What one stream in progress costs
did not change; it was 1.1 KiB when `giong-http/README.md` last measured it, and is 1.51 on
`main` too, so it grew before this change, and is the next thing to look at in HTTP/2.

## Throughput (bench/tls.sh endpoints)

A copy of `bench/tls.sh` whose list of servers names the two builds of `bench/giong-http`
(with TLS, `GIONG_KTLS=off`, and without), the rest unchanged: one core for the server, three
for `wrk`, 50 connections, 5 s per measurement after 2 s of warm-up, a fresh server for each
round. The builds alternate within every round, so each round gives one pair; the table has
the median over the pairs of the change's requests per second against main's.
[`throughput/raw.csv`](throughput/raw.csv) has every measurement, with the run and the order
the builds ran in.

| endpoint | with TLS | without TLS |
|---|---:|---:|
| `json` | +2.2% (16 pairs) | +0.3% (16 pairs) |
| `users` | **+9.9%** (6 pairs, 5 up) | **+6.2%** (6 pairs, all up) |
| `bytes64k` | −0.4% (6 pairs) | −1.0% (6 pairs) |
| `file64k` | −0.7% (26 pairs) | +0.5% (16 pairs) |
| `handshake` | −0.7% (74 pairs) | |

The rounds of one build spread over ±3 to ±8% on this VM, so only `users` moved by more than
the noise: its handler's future is the largest of these, and it is now made in its heap block
instead of copied there. `handshake` (a resumed TLS 1.3 handshake and one request per
connection) needed more pairs to settle: with main always first (runs A and B) the change
measured −2.6%, with the change first (run C) −0.3%, and in runs whose order alternates or
rotates −1.3% (run D, 24 pairs) and +0.2% (run E, 24 rotations of main, the first version of
the change and the final one; the first version measured +1.2%). The server's instructions
per connection on that path are 0.06% fewer with the change (`callgrind.sh <binary>
handshake`: 1,286,445 against 1,287,213), and callgrind's cache simulation
counts 1.1% fewer instruction-cache misses and 4 to 7% fewer data-cache misses on it, so the
−0.7% is taken for the VM's noise; it is to be measured again on bare metal.
