# Idle HTTPS memory: rustls' unbuffered API

rustls in the process, before and after giong-tls moved from rustls' buffered connection
to its unbuffered API (changelog.d/perf-tls-idle-memory.md). The buffered connection
keeps a 4 KiB receive buffer for the life of every connection; with the unbuffered API
received bytes are processed from a scratch buffer each thread shares, and a connection
keeps bytes of its own only while it holds part of a record. Outgoing records go through
scratch buffers each thread shares too, where every stream kept its own.

Machine: 4-vCPU cloud VM (Intel Xeon), Linux 6.18, rustc 1.97.0, 2026-11-26. "main-1cb2988"
is `main` at 1cb2988; "unbuffered" is the change on top of it.

## Resident memory, 8,000 idle connections

`bench/tls-idle.sh 3 8000` (`BINS` naming the two builds of `bench/giong-http`): one worker
on CPU 3, an OpenSSL client (`tls-idle/hold.py`) on the others opening the connections one
by one, each with a full TLS 1.3 handshake and one keep-alive `GET /json`, then idle.
[`raw.csv`](raw.csv), [`summary.txt`](summary.txt).

| | anonymous memory per connection | resident set per connection | resident set with 8,000 |
|---|---:|---:|---:|
| main-1cb2988 | 10.92 KiB | 10.96 KiB | 89.8 MiB |
| unbuffered | **6.17 KiB** | **6.19 KiB** | **52.8 MiB** |

All three rounds of each gave the same numbers to the hundredth.

## Heap per connection and instructions per request (bench/instr)

`scripts/instr.py measure --cases https,idle-https --rounds 3` on both trees, same VM.
[`instr-main-1cb2988.csv`](instr-main-1cb2988.csv), [`instr-unbuffered.csv`](instr-unbuffered.csv).

| case | metric | main-1cb2988 | unbuffered | change |
|---|---|---:|---:|---:|
| `idle-https` io_uring | heap bytes per connection | 10,833.5 | 6,069.3 | −44.0% |
| `idle-https` epoll | heap bytes per connection | 10,835.8 | 6,085.9 | −43.8% |
| `idle-https` io_uring | resident bytes per connection | 11,266.0 | 6,479.9 | −42.5% |
| `idle-https` epoll | resident bytes per connection | 11,147.3 | 6,385.7 | −42.7% |
| `https` io_uring | instructions per request | 33,883.3 | 33,486.2 | −1.2% |
| `https` epoll | instructions per request | 34,953.0 | 34,577.9 | −1.1% |
| `https` both | allocations per request | 17.0 | 17.0 | 0 |

`idle-https` allocations per connection, handshake included, went from 121.2 to 122.2;
the heap per connection shows that none of the extra one stays.

## Throughput (bench/tls.sh)

`bench/tls.sh 5 5` with the two builds as servers, interleaved, server on one core, wrk with
three threads and 50 connections on the other three. [`throughput/raw.csv`](throughput/raw.csv),
[`throughput/summary.txt`](throughput/summary.txt).

| endpoint | main-1cb2988 req/s | unbuffered req/s | change | server CPU per request |
|---|---:|---:|---:|---|
| `json` (keep-alive) | 135,248 | 137,319 | +1.5% | 7.31 → 7.20 µs |
| `users` (8.5 KiB) | 42,705 | 46,047 | +7.8% | 23.18 → 21.54 µs |
| `handshake` (new connection each request) | 7,353 | 7,630 | +3.8% | 133.4 → 128.2 µs |
| `bytes64k` | 29,029 | 29,687 | +2.3% | 33.80 → 33.29 µs |

Medians of five rounds; no endpoint lost. One `json` round of the change was low (83,846,
the others 135,000 to 141,700), within the 10-15% round-to-round spread this VM shows.
