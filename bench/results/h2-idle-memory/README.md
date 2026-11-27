# Idle HTTP/2 memory: a task of its own, smaller receive and write futures

giong-http, giong-tls and giong before and after the change of
`changelog.d/perf-h2-idle-memory.md`. Three parts:

- **An HTTP/2 connection goes on in a task of its own.** A connection is accepted into a
  task that reads its first bytes and serves HTTP/1.1. Until this change, once the TLS
  handshake picked `h2` or the HTTP/2 preface arrived, that task went on serving HTTP/2 from a
  heap block of its own (`HeapFuture`), so an HTTP/2 connection kept both for as long as it
  lived: the HTTP/1.1 task (992 to 2,680 bytes, most of it state HTTP/2 never uses) and the
  HTTP/2 state. Now the accepting task spawns the HTTP/2 loop as a new task, handing over the
  stream, the configuration, the connection's place in the server's count and the bytes read
  so far, and ends. The new task inherits the worker's budget like any spawned task, and the
  spawn is kept out of line (`#[cold]`), so the HTTP/1.1 connection's poll function stays
  small enough to be inlined into the executor's loop. `Server::serve_connection`, which has
  no accept loop to hand the connection to, still serves it in the future it returns.
- **A TLS receive no longer holds room for the handshake's sends.** On an established stream
  a receive only ever sends a key update or an alert, which go out without waiting (as they
  already did); the room the handshake's sends needed (504 bytes) was in every waiting
  receive. `TlsStream::recv` is 136 bytes instead of 640.
- **Writes keep their arguments once.** An `async fn` keeps its arguments twice in its
  future, as received and moved into its body; the stream writes of the core
  (`TcpStream::write_all` and the others) and of giong-tls are now plain functions returning
  an `async move` block. A `TcpStream::write_all` future is 240 bytes instead of 296, a
  `TlsStream::write_all` future 496 instead of 624 (the rare sends of kernel TLS before a
  write, a key update, are boxed).

Machine: 4-vCPU cloud VM (Intel Xeon), Linux 6.18, rustc 1.97.0, valgrind 3.22, 2026-11-27.
"main" is `main` at a1b2db5 (#481), measured on da127ad, the same code (the two differ only in
tests); "change" is the final version of the change merged with it. The resident-memory runs
(`tls-*-final`, `h2-memory-final`) were taken one commit before the final one, which moves
the HTTP/2 hand-over out of line and changes no size: `bench/instr`'s idle rows are the same
to the byte. `*-ed6df07` are the first step alone (the task of its own and the smaller
receive, without the smaller writes).

## Size of a connection's future

`-Zprint-type-sizes` of `bench/instr`, release build, for the handler of `bench/instr`'s
`http1` case. A task is one heap block, allocated when the connection is accepted.

| what an idle connection keeps | main | change |
|---|---:|---:|
| HTTP/1.1: the connection's task | 992 bytes | **936 bytes** |
| HTTPS: the connection's task | 2,680 bytes | **2,536 bytes** |
| HTTP/2: the accepting task + `h2::serve` in a heap block | 992 + 1,616 bytes | |
| HTTP/2: its own task | | **1,848 bytes** |
| HTTP/2 over TLS: the accepting task + `serve_h2` in a heap block | 2,680 + 4,112 bytes | |
| HTTP/2 over TLS: its own task | | **3,504 bytes** |
| `TlsStream::recv` (in every waiting HTTPS and HTTP/2-over-TLS loop) | 640 bytes | 136 bytes |
| `TlsStream::write_all` | 624 bytes | 496 bytes |
| `TcpStream::write_all` (an idle HTTP/1.1 connection keeps one) | 296 bytes | 240 bytes |

## Heap per idle connection and instructions per request (bench/instr)

`scripts/instr.py measure --rounds 3` on both trees.
[`instr-base-da127ad/raw.csv`](instr-base-da127ad/raw.csv),
[`instr-final/raw.csv`](instr-final/raw.csv). `idle-h2-tls` is a new case (HTTP/2 over TLS,
ALPN `h2`); [`instr-main-idle/raw.csv`](instr-main-idle/raw.csv) has the idle cases of main
measured with it (`bench/instr` of the change on main's tree). CI's run of the change
(45925229409) gave the same heap bytes.

| case | metric | main | change | change |
|---|---|---:|---:|---:|
| `idle-http1` io_uring / epoll | heap bytes per connection | 1,318.3 / 1,384.9 | **1,262.3 / 1,328.9** | **−4.2% / −4.0%** |
| `idle-h2` io_uring / epoll | heap bytes per connection | 3,317.9 / 3,334.5 | **2,517.9 / 2,534.6** | **−24.1% / −24.0%** |
| `idle-https` io_uring / epoll | heap bytes per connection | 5,285.3 / 5,301.9 | **5,141.3 / 5,157.9** | **−2.7% / −2.7%** |
| `idle-h2-tls` io_uring / epoll | heap bytes per connection | 9,682.8 / 9,699.5 | **6,396.8 / 6,413.5** | **−33.9% / −33.9%** |
| `idle-http1` io_uring / epoll | resident bytes per connection | 1,447.9 / 1,503.2 | 1,384.4 / 1,437.7 | −4.4% / −4.4% |
| `idle-h2` io_uring / epoll | resident bytes per connection | 3,641.3 / 3,625.0 | 2,840.6 / 2,793.5 | −22.0% / −22.9% |
| `idle-https` io_uring / epoll | resident bytes per connection | 5,689.3 / 5,591.0 | 5,548.0 / 5,447.7 | −2.5% / −2.6% |
| `idle-h2-tls` io_uring / epoll | resident bytes per connection | 10,268.7 / 10,149.9 | 6,975.5 / 6,866.9 | −32.1% / −32.3% |
| `https` io_uring / epoll | instructions per request | 31,605.1 / 32,684.9 | **30,681.0 / 31,774.8** | **−2.92% / −2.78%** |
| `h2-streams` io_uring / epoll | instructions per ten requests | 224,731.1 / 226,037.2 | **222,155.3 / 223,508.9** | **−1.15% / −1.12%** |
| `http1-client` io_uring / epoll | instructions per request | 19,973.4 / 21,068.7 | 19,847.3 / 20,948.7 | −0.63% / −0.57% |
| `tcp-echo` io_uring / epoll | instructions per echo | 3,214.0 / 4,152.4 | 3,196.0 / 4,134.4 | −0.56% / −0.43% |
| `h2` io_uring / epoll | instructions per request | 28,185.2 / 29,324.0 | 28,074.6 / 29,195.6 | −0.39% / −0.44% |
| `postgres-query` io_uring / epoll | instructions per query | 9,568.5 / 10,396.7 | 9,541.5 / 10,367.7 | −0.28% / −0.28% |
| `mysql-query` io_uring / epoll | instructions per query | 13,065.6 / 13,894.7 | 13,033.6 / 13,860.7 | −0.24% / −0.24% |
| `redis-get` io_uring / epoll | instructions per request | 7,763.1 / 8,541.8 | 7,745.1 / 8,521.8 | −0.23% / −0.23% |
| `http1` io_uring / epoll | instructions per request | 8,172.6 / 9,083.5 | 8,155.7 / 9,066.6 | −0.21% / −0.19% |
| `web` io_uring / epoll | instructions per request | 10,999.4 / 11,916.5 | 10,988.4 / 11,893.5 | −0.10% / −0.19% |

The other cases (`spawn`, `yield`, `oneshot`, `channel`, `timeout`, `udp-echo`,
`file-read`) are the same to the decimal, and allocations are unchanged in every case. The
heap per idle connection moved by the sizes above: by exactly 56 bytes for HTTP/1.1
(992 − 936) and 144 for HTTPS (2,680 − 2,536); by 800 for HTTP/2, of which the tasks account
for 760 (992 + 1,616 − 1,848); and by 3,286 for HTTP/2 over TLS, where they account for 3,288
(2,680 + 4,112 − 3,504).

A first version of the change spawned the HTTP/2 task inline in the HTTP/1.1 connection's
future: `http1` measured +0.11% and `web` +0.36% (io_uring). Callgrind showed the
connection's poll function no longer inlined into the executor's loop
(`LocalTask::run`), about 50 instructions a request; with the spawn in a `#[cold]` function of
its own both cases are below main.

## Instructions per response (bench/giong-http, callgrind)

`bench/results/idle-conn-memory/callgrind.sh` on both builds of `bench/giong-http`, two runs
of each, interleaved ([`callgrind.txt`](callgrind.txt)): the server's instructions per
`GET /json` on a keep-alive connection, and per connection for a resumed TLS 1.3 handshake
and one request (`handshake`, rustls in the process).

| path | main | change | change |
|---|---:|---:|---:|
| `/json` | 6,390.2 | **6,377.6** | **−0.20%** |
| `handshake` | 1,284,616 | **1,283,703** | −0.07% |

## Resident memory, 8,000 idle HTTPS connections (bench/tls-idle.sh)

`bench/tls-idle.sh 3 8000`, and `PROTOCOL=h2 bench/tls-idle.sh 3 8000` for HTTP/2 over TLS
(`BINS` naming the builds of `bench/giong-http`): one worker, an OpenSSL client opening the
connections one by one, each with a full TLS 1.3 handshake and one keep-alive request, then
idle. [`tls-h2-base-and-ed6df07/`](tls-h2-base-and-ed6df07/summary.txt),
[`tls-h2-final/`](tls-h2-final/summary.txt),
[`tls-http1-base-and-ed6df07/`](tls-http1-base-and-ed6df07/summary.txt),
[`tls-http1-final/`](tls-http1-final/summary.txt).

| protocol | | anonymous memory per connection | resident set per connection | resident set with 8,000 |
|---|---|---:|---:|---:|
| HTTP/2 | main | 9.81 KiB | 9.85 KiB | 81.2 MiB |
| HTTP/2 | first step (ed6df07) | 6.72 KiB | 6.76 KiB | 57.1 MiB |
| HTTP/2 | change | **6.59 KiB** | **6.62 KiB** | **56.1 MiB** |
| HTTP/1.1 | main | 5.40 KiB | 5.44 KiB | 46.7 MiB |
| HTTP/1.1 | first step (ed6df07) | 5.39 KiB | 5.43 KiB | 46.6 MiB |
| HTTP/1.1 | change | **5.26 KiB** | **5.30 KiB** | **45.7 MiB** |

All three rounds of each gave the same numbers to within 0.01 KiB (the resident set within
0.1 MiB). An idle HTTP/2 connection over TLS now costs 1.25 times an HTTP/1.1 one, where it
cost 1.8 times.

## HTTP/2: 5,000 connections, one worker (bench/h2.sh)

`PARTS=memory MEM_SERVERS=giong-http bench/h2.sh 3` in each tree: 5,000 connections over
HTTP/2 with prior knowledge from `bench/hold`, one worker. "Idle": one request answered on
each connection; "pending": one request still being handled; "per stream": each further
request in progress on a connection (ten in all).
[`h2-memory-base-da127ad/`](h2-memory-base-da127ad/memory.csv),
[`h2-memory-ed6df07/`](h2-memory-ed6df07/memory.csv),
[`h2-memory-final/`](h2-memory-final/memory.csv).

| | idle | pending | per stream |
|---|---:|---:|---:|
| main | 3.48 KiB | 4.86 KiB | 1.50 KiB |
| first step (ed6df07) | 2.72 KiB | 4.12 KiB | 1.51 KiB |
| change | **2.66 KiB** | **4.06 KiB** | 1.51 KiB |

All three rounds gave the same values to within 0.01 KiB, which is also all a stream in
progress moved by (1.502 to 1.509 KiB from the ten-stream totals): what a stream costs is the
stream's own state, which this change does not touch, and is the next thing to look at.

## Not measured here

Requests per second: the rounds of one build spread over ±3 to ±8% on this VM, more than any
effect the instruction counts above suggest (at most −2.9%, for `https`); it is on the list
for the bare-metal runs.
