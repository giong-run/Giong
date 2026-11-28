# HTTP/3 response priorities (RFC 9218)

`Server::serve_h3` before and after the change of `changelog.d/h3-priorities.md`: responses
are sent in the order of their urgency (the request's `priority` header field, a
PRIORITY_UPDATE frame, or the response's own `priority` field), and responses of equal urgency
share the connection in turn.

Machine: 4-vCPU cloud VM (Intel Xeon @ 2.10 GHz), Linux 6.18, rustc 1.100.1, valgrind 3.22,
2026-11-28. "base" is commit c7ba8a5 (two-step GOAWAY, the commit this change is built on);
"priorities" is c4fe647.

## An urgent response on a busy connection

`measure_urgent_response_times` in `giong-http/tests/h3_priority.rs` (ignored by default; run
with `cargo test --release -p giong-http --features unstable-http3,client --test h3_priority
measure_urgent_response_times -- --ignored --nocapture`). The client and the server talk
through a relay that limits the server-to-client direction to 8 MB/s, adds 10 ms of delay each
way, and drops datagrams that would queue more than 200 ms. The client asks for four 1 MiB
responses on one connection, and 100 ms later for one more response, either without a
`priority` field (urgency 3, like the others) or with `priority: u=0`. The time is from that
last request to its whole response. Three rounds: [`latency-raw.txt`](latency-raw.txt).

| late response | no priority field (median) | `priority: u=0` (median) | change |
|---|---:|---:|---:|
| 16 KiB | 33.9 ms | 24.9 ms | **−27%** |
| 256 KiB | 191.1 ms | 56.6 ms | **−70%** |
| 1 MiB | 655.2 ms | 157.4 ms | **−76%** |

An urgent 1 MiB response needs 131 ms on an 8 MB/s link plus a 20 ms round trip, so 157 ms is
6 ms from what the link allows. Without the field it shares the link with the four other
responses and ends with them.

The loss: the four 1 MiB responses wait while the urgent one goes first. Their mean time
changes from 620.8 to 621.3 ms (+0.1%) with a 16 KiB urgent response, 652.6 to 653.0 ms
(+0.1%) with 256 KiB, and 750.0 to 754.4 ms (+0.6%) with 1 MiB. The link moves the same bytes
in both cases; only their order changes.

## Instructions per request (callgrind)

The `h3` example under callgrind, 100 and 600 GET requests of `/` on one connection with no
`priority` field (aioquic client), as in [`../h3-qpack-dynamic`](../h3-qpack-dynamic): the
server's instructions per request are (Ir after 600 − Ir after 100) / 500. Three rounds,
interleaved: [`callgrind-raw.txt`](callgrind-raw.txt), script
`../h3-qpack-dynamic/callgrind.sh`.

| round | base | priorities |
|---|---:|---:|
| 1 | 58,716 | 59,805 |
| 2 | 59,024 | 57,770 |
| 3 | 58,222 | 58,901 |
| median | 58,716 | 58,901 |

The totals move ±2% from one run to the next in both directions, with the number of packets
and acknowledgements in each run, so they cannot show a cost this small. Per function
([`callgrind-functions.txt`](callgrind-functions.txt), from
[`instr-diff.sh`](instr-diff.sh)), the change costs the same in every round:

| function | instructions per request, all three rounds |
|---|---:|
| `h3::server::respond` (the response's urgency, setting the stream's priority) | +53 |
| `h3::server::exchange` (the request's urgency, pending PRIORITY_UPDATEs) | +15 |
| `h3::server::Reader::header` (the `priority` field) | +5 |
| **total** | **+73 (+0.12%)** |

Every other function's change differs in sign from round to round (quinn-proto, the QUIC
driver, malloc and memcpy), following the packet counts.
