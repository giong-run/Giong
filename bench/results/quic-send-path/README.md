# giong-quic's send path: no copy of each datagram batch, sent in the driver's turn

giong-quic before and after the change of `changelog.d/perf-quic-send-path.md`. The 1.5.x
bare-metal run (`bench/results/bare-metal-epyc-4345p/linux-7.0/1.5.x/`, section 6) left two
QUIC losses against quinn: request/response on one worker (498k against 680k req/s) and bulk
transfer on one worker (10.5 against 14.8 Gbit/s). Those rows ran `bench/quic`'s own
quinn-proto engine on giong (`server-proto-giong`), not the giong-quic crate that users and
giong-http's HTTP/3 run. This folder starts with giong-quic itself: a server with the same
workload as `server-quinn` (`bench/quic/src/bin/server-giong-quic.rs`: a task per connection
and per stream, each answer written with `write_all`).

Three costs on every transmit, found with callgrind and `perf record` on the round-trip case:

- **A copy of every datagram batch.** quinn-proto builds a transmit (up to 10 datagrams for
  GSO) in the endpoint's scratch buffer, and the driver copied it into a spare buffer to send.
  The scratch buffer is now swapped for a spare one instead: the datagrams go out from the
  buffer they were built in.
- **A task per transmit.** Every send was spawned as a task of its own. The driver now polls
  each send once in its own turn: giong sends a datagram at once when the socket has room, so
  the send completes in that poll, and only a send that has to wait becomes a task.
- **A timer per wait.** The driver waited for its connections' next deadline with a new
  `sleep_until` every turn: a timer inserted in the wheel and removed again, on almost every
  turn (acknowledgement and loss timers move all the time). It now keeps one hidden
  `giong::time::Alarm`, armed from one wait to the next, which only retargets its entry (a
  later deadline only changes the deadline the entry keeps).

Machine: 4-vCPU cloud VM (Intel Xeon @ 2.10 GHz), Linux 6.18, rustc 1.97.0, valgrind 3.22,
2026-11-28 ([`env.txt`](env.txt)). "base" is `server-giong-quic` built on main (6e9c2f5);
"new" is the same server with this change. Client: `bench/quic`'s quinn client.

## Instructions per round trip (callgrind)

One connection, one stream at a time, 100-byte answers: the server's instructions per
request, as the difference of a 12 s and a 4 s run under callgrind divided by the difference
in requests (so start-up and the handshake cancel out). Three runs each, interleaved:
[`callgrind-rr.csv`](callgrind-rr.csv), script [`callgrind.sh`](callgrind.sh).

| server | runs | median |
|---|---|---:|
| giong-quic, base | 30,567 / 31,005 / 30,280 | 30,567 |
| giong-quic, new | 27,922 / 28,308 / 29,500 | **28,308 (−7.4%)** |
| quinn 0.11 (Tokio) | 29,156 / 29,354 / 30,093 | 29,354 |

giong-quic used 4% more instructions than quinn per round trip before, and 3.6% fewer now.
A new test (`a_round_trip_costs_few_driver_turns_and_sends_at_once` in `giong-quic/src/proto.rs`)
keeps it there: a round trip costs at most 4 driver turns over both endpoints, and a socket
with room takes every send without a task.

## Syscalls and io_uring completions per request

`perf stat` on the server over 4 s of each measured window (`raw_syscalls:sys_enter`,
`io_uring:io_uring_submit_req`, `io_uring:io_uring_complete`), divided by the requests (or the
MiB sent) in that window; server on CPUs 0-1, client on 2-3; median of 3 rounds:
[`syscalls-raw.csv`](syscalls-raw.csv), script [`syscalls.sh`](syscalls.sh). `rr` is 8
connections × 16 streams with 100-byte answers, `rr1` one stream at a time, `bulk` 4
connections × 16 MiB.

| case | per | giong-quic base | giong-quic new | quinn |
|---|---|---:|---:|---:|
| `rr` | syscalls / request | 0.611 | **0.482 (−21%)** | 0.579 |
| `rr` | io_uring completions / request | 0.945 | 1.002 | — (epoll) |
| `rr1` | syscalls / request | 2.04 | 2.04 | 4.92 |
| `bulk` | syscalls / MiB | 88.2 | 86.3 | **78.0** |
| `bulk` | io_uring completions / MiB | 14.0 | 13.9 | — |

Under load giong-quic now makes fewer syscalls per request than quinn (a send that finds the
socket free goes out with one `sendmsg` from the driver's turn, receives are multishot
completions). Bulk still makes 10.6% more syscalls per MiB than quinn: **a loss**, below.

## Acknowledgements (protocol, not changed here)

The client counts the datagrams and ACK frames it receives from the server over the window
(`srv_dgrams_per_req`, `srv_acks_per_req`, new in `bench/quic/src/bin/client.rs`). Both
servers run quinn-proto with the same transport settings, so ACK frequency is the protocol's,
but how often the driver asks quinn-proto for transmits changes how datagrams are filled:

| case | server datagrams / request (base / new / quinn) | ACK frames / request (base / new / quinn) |
|---|---|---|
| `rr` | 0.424 / 0.391 / 0.341 | 0.281 / 0.255 / 0.203 |
| `rr1` | 1.008 / 1.008 / 1.008 | 1.00 / 1.00 / 1.00 |
| `bulk` (per 16 MiB answer) | 12,066 / 12,050 / 12,035 | 20.0 / 19.7 / 41.8 |

Under load giong-quic sends 15% more datagrams per request than quinn (and 26% more ACK
frames): its driver hands answers to quinn-proto in smaller groups, so fewer of them share a
datagram. That is the next thing to look at for `rr`. In bulk transfer giong-quic acknowledges
the client half as often as quinn does. No ACK-frequency change (RFC draft
`ack-frequency`, or a different `ack_eliciting_threshold`) is part of this change; one would
be a protocol change of its own.

## Time on the VM

5 rounds, 5 s each, server on CPUs 0-1, client on 2-3, all four servers interleaved per round:
[`vm-ab-raw.csv`](vm-ab-raw.csv), script [`ab.sh`](ab.sh). `quinn-copy` is quinn writing each
answer with `write_all` from a slice, the call giong-quic has (`server-quinn --copy 1`);
plain quinn writes `Bytes` with `write_chunk`, without a copy.

| case | metric (median) | base | new | quinn | quinn-copy |
|---|---|---:|---:|---:|---:|
| `rr` | req/s | 202,800 | 202,323 | 201,941 | 202,027 |
| `rr` | server µs CPU / request | 4.67 | 4.63 | 4.69 | 4.70 |
| `bulk` | Gbit/s | 5.53 | **6.01 (+8.8%)** | 7.86 | 5.77 |
| `bulk` | server ns CPU / KiB | 1,401 | **1,319 (−5.9%)** | 1,037 | 1,414 |

On this VM the server is CPU-bound in `rr` (93–98% of a core) and the client shares the
machine, so the four are level within the noise there; the 7.4% fewer instructions show as
less CPU only within that noise. `vm-rr4-raw.csv` (8 × 4 streams) is the same.

## Losses, stated plainly

- **Bulk transfer: quinn sends 16 MiB answers with 21% less CPU per KiB than giong-quic**
  (1,037 against 1,319 ns/KiB) and 31% more Gbit/s. `perf record` puts `memmove` at 22.7% of
  giong-quic's server against 4.4% of quinn's: `write_all(&[u8])` copies every answer into
  quinn-proto's send buffer, and quinn-proto later reads it again, cold, to build packets.
  quinn writing the same way (`quinn-copy`) costs as much or more than giong-quic (1,414
  ns/KiB). The remaining gap is quinn's `write_chunk(Bytes)`, which hands over owned bytes
  without a copy; giong-quic has no such call, and adding one is new API, outside this change.
- **The bare-metal rows are not closed by this change, and not explained yet.** On bare metal
  the giong engine used *less* CPU per request than quinn (1.11–1.24 against 1.41–1.46 µs)
  and had a lower p50 (105 against 177 µs), yet ran at 46–75% of its core and reached 386k to
  680k req/s from round to round: with 128 streams always in flight, a mean latency of about
  250 µs against a p99 of 263 µs means about 1% of requests waited many milliseconds. That
  stall cannot be reproduced here: on this VM every server is CPU-bound in every round-trip
  setup tried. The next bare-metal run has rows for it in `bench/quic-2host.sh`: `giong-quic`,
  `giong-quic-nopin` (is it the worker pinned on the core that takes the NIC's interrupts?)
  and `quinn-copy`, with the client's p99.9, maximum and mean.
