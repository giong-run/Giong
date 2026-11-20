# UDP echo: giong against Tokio and the kernel's best

`bench/udp-echo.sh` (see `bench/udp-echo/src/main.rs`): a UDP echo server pinned to one core,
64-byte datagrams, the client in another network namespace over veth on the other cores,
keeping 256 datagrams in flight. 5 rounds of 8 seconds per mode; medians with the range.
Cloud VM, 4 vCPUs (Intel Xeon @ 2.10GHz), Linux 6.18. `raw.csv` in each directory has
every round.

- `kernel`: `recvmmsg`/`sendmmsg`, 32 datagrams per call, no runtime: the ceiling for
  kernel sockets.
- `giong`, `giong-epoll`: `giong::net::UdpSocket` on one worker, io_uring or epoll.
- `tokio`: `tokio::net::UdpSocket` on a current-thread runtime.
- `TASKS`: echo tasks sharing the one socket (16, or 1 receive loop).

`before-*` is the runtime of main 8606db1, built with the same benchmark; `after-*` is this
branch (19450d1). They were run one after the other in one session; Tokio and the kernel-only
server ran in both, and the table shows them from the `after` run (`before`: Tokio 289,303
and 303,678, kernel 367,574 and 390,034).

Echoes per second (median):

| | 16 tasks | 1 task |
|---|---:|---:|
| giong io_uring, before | 258,680 | 285,536 |
| giong io_uring, after | **328,288** | **299,703** |
| giong epoll, before | 239,361 | 258,380 |
| giong epoll, after | **284,118** | **284,248** |
| Tokio | 277,780 | 258,803 |
| kernel `recvmmsg`/`sendmmsg` × 32 | 386,042 | 357,388 |

Server CPU per echo (its process plus softirq on its core), µs:

| | 16 tasks | 1 task |
|---|---:|---:|
| giong io_uring, before | 5.28 | 4.64 |
| giong io_uring, after | 4.24 | 4.79 |
| giong epoll, before | 5.90 | 5.43 |
| giong epoll, after | 5.07 | 5.07 |
| Tokio | 5.24 | 5.74 |
| kernel × 32 | 3.62 | 3.92 |

What changed, and what each part was worth on the way (2-3 round runs, same machine):

1. io_uring, tasks sharing a socket: the multishot receive had one wake-up slot, so of 16
   waiting tasks only the last was woken and the server made one `io_uring_enter` per
   datagram (769k system calls for 249k echoes a second). Each datagram now wakes a task of
   its own: 21k system calls a second, 258k → 305k echoes.
2. `sendto`/`recvfrom` instead of `sendmsg`/`recvmsg` with a header allocated per datagram
   (and, on io_uring, `IORING_OP_SEND` with a destination address from Linux 6.1).
3. Sends go out within the call (`sendto` with `MSG_DONTWAIT`) on both drivers; only a full
   socket falls back to an operation. With io_uring a send no longer waits for its
   completion, which with one task per socket cost a turn of the event loop per datagram
   (274k → 349k with one task). Receives stay multishot, so an echo is one system call.
4. epoll receives up to 16 datagrams per `recvmmsg` while half the buffer pool stays free:
   one task 274k → 321k.

Variation between rounds on this VM is about ±10%. giong io_uring is ahead of Tokio in every
round. giong epoll is level with Tokio: ahead in the medians (2% with 16 tasks, 10% with one),
behind in two of five 16-task rounds. The epoll driver passes Tokio clearly with the next
change (sends of one turn in one `sendmmsg`, busy sockets out of the epoll set; see
`vm/epoll-deep/`).

## Bare metal

The same benchmark on an AMD EPYC 4345P (8 cores, SMT on), Linux 7.0.0-43, server on CPU 1,
client on CPUs 2-3, with this branch at f3310e5, on 2026-11-19
(`bench/results/bare-metal-epyc-4345p/linux-7.0/post-soak/udp-udp-epoll-tasks16/` and
`-tasks1/`, `raw.csv` with every round; the runtime before the send queue, 997b290, in
`udp-udp-faster-*`). The server core is saturated: the kernel × 32 server spends 0.99 µs of
process CPU per echo, and the runtimes 1.08 to 1.17.

| Echoes per second, median (min–max) | 16 tasks | 1 task |
|---|---:|---:|
| giong epoll | **920,836** (904,872–926,239) | **892,565** (881,529–906,552) |
| giong io_uring | **902,459** (895,390–914,320) | 875,632 (863,835–927,756) |
| Tokio | 870,488 (855,699–877,000) | 863,001 (851,625–897,835) |
| kernel × 32 | 1,009,532 | 1,028,443 |

Server process CPU per echo (median, µs): 16 tasks epoll 1.085, io_uring 1.107, Tokio 1.148;
1 task 1.119, 1.141, 1.157.

Rounds in which giong beat Tokio: with 16 tasks 5 of 5 on both drivers (5.8% and 3.7% in the
median); with one task 4 of 5 on epoll and 3 of 5 on io_uring (3.4% and 1.5%, one of those
wins by 0.1%). So 16 tasks is a clear win and one task a tie within noise: every runtime moves
between two levels from round to round (about 870k and 930k). Before the send queue (997b290)
giong lost to Tokio with io_uring and 16 tasks (856,470 against 870,331) and with epoll and one
task (874,499 against 882,456).

`perf stat` on the server for 8 s (`post-soak/udp-perf/`): with 16 tasks giong io_uring made
328,444 `io_uring_enter` and 547,405 `sendmmsg` calls, about 0.12 per echo, where Tokio made
6,857,450 `sendto`; with one task giong makes one `sendto` per echo
(6,720,819) and 203,662 `io_uring_enter`. Most of an echo's cost is the kernel's UDP and veth
path, including 6.6% in nftables (the host's firewall on veth, the same for every server).

To reproduce:

    sudo TASKS=16 bench/udp-echo.sh 5 8
    sudo TASKS=1  bench/udp-echo.sh 5 8
