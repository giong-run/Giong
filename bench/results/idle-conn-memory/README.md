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
value.

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
| `idle-http1` io_uring / epoll | resident bytes per connection | 2,187.3 / 2,242.6 | 1,452.0 / 1,507.3 | −33.6% / −32.8% |
| `idle-h2` io_uring / epoll | resident bytes per connection | 4,907.0 / 4,862.0 | 3,743.7 / 3,661.8 | −23.7% / −24.7% |
| `idle-https` io_uring / epoll | resident bytes per connection | 6,479.9 / 6,381.6 | 5,697.5 / 5,599.2 | −12.1% / −12.3% |
| `http1` io_uring / epoll | instructions per request | 8,293.7 / 9,204.5 | 8,212.7 / 9,123.6 | −0.98% / −0.88% |
| `http1-client` io_uring / epoll | instructions per request | 21,851.3 / 22,946.6 | 21,770.4 / 22,865.6 | −0.37% / −0.35% |
| `https` io_uring / epoll | instructions per request | 33,500.1 / 34,580.0 | 33,430.2 / 34,516.0 | −0.21% / −0.19% |
| `web` io_uring / epoll | instructions per request | 11,263.4 / 12,174.5 | 11,242.4 / 12,147.6 | −0.19% / −0.22% |
| `h2` io_uring / epoll | instructions per request | 29,743.6 / 30,875.0 | 29,725.4 / 30,854.4 | −0.06% / −0.07% |
| `h2-streams` io_uring / epoll | instructions per ten requests | 240,531.8 / 241,812.9 | 240,590.1 / 241,777.2 | +0.02% / −0.01% |

Allocations are unchanged in every case: the blocks the worker reuses for `HeapFuture` are
not new allocations once warm. `h2-streams` is the median of eight rounds (the three above
and five more, [`instr-h2-streams-main-81588a2.csv`](instr-h2-streams-main-81588a2.csv),
[`instr-h2-streams-change.csv`](instr-h2-streams-change.csv)); its rounds spread over
±0.03 to ±0.04% on either tree (io_uring: 240,449 to 240,590 on main, 240,474 to 240,654
with the change), so +0.02% and −0.01% are within the noise of the measurement. None of the HTTP/2
request path changed.

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

THROUGHPUT_SECTION
