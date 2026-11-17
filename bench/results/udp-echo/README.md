# UDP echo: giong against Tokio and the kernel's best

`bench/xdp.sh` (see `bench/xdp/src/main.rs`): a UDP echo server pinned to one core,
64-byte datagrams, the client in another network namespace over veth on the other cores,
keeping 256 datagrams in flight. 5 rounds of 8 seconds per mode; medians with the range.
Cloud VM, 4 vCPUs (Intel Xeon @ 2.10GHz), Linux 6.18. `raw.csv` in each directory has
every round.

- `kernel`: `recvmmsg`/`sendmmsg`, 32 datagrams per call, no runtime: the ceiling for
  kernel sockets.
- `giong`, `giong-epoll`: `giong::net::UdpSocket` on one worker, io_uring or epoll.
- `tokio`: `tokio::net::UdpSocket` on a current-thread runtime.
- `TASKS`: echo tasks sharing the one socket (16, or 1 receive loop).

`before-*` is the runtime of `bench/xdp-ceiling` (main 4e7df8b), built with the same
benchmark; `after-*` is this branch. Both were run in the same session.

Echoes per second (median):

| | 16 tasks | 1 task |
|---|---:|---:|
| giong io_uring, before | 265,836 | 265,181 |
| giong io_uring, after | **330,254** | **350,457** |
| giong epoll, before | 261,906 | 241,574 |
| giong epoll, after | **302,277** | **301,907** |
| Tokio | 286,618 | 276,996 |
| kernel `recvmmsg`/`sendmmsg` × 32 | 395,495 | 386,937 |

Server CPU per echo (its process plus softirq on its core), µs:

| | 16 tasks | 1 task |
|---|---:|---:|
| giong io_uring, after | 4.31 | 4.04 |
| giong epoll, after | 4.79 | 4.75 |
| Tokio | 5.10 | 5.27 |
| kernel × 32 | 3.54 | 3.68 |

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

Variation between rounds on this VM is about ±8%. giong io_uring is ahead of Tokio in every
round. giong epoll is ahead in the medians, but its range overlaps Tokio's (16 tasks:
295-341k against 274-300k), so on epoll the lead is about 5% (16 tasks) to 9% (1 task),
not more.
To reproduce:

    sudo TASKS=16 MODES="kernel giong giong-epoll tokio" bench/xdp.sh 5 8
    sudo TASKS=1  MODES="kernel giong giong-epoll tokio" bench/xdp.sh 5 8
