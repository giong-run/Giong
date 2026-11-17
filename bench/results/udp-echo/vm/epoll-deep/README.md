# UDP echo: the epoll path at the kernel's ceiling

Same harness and machine as `../README.md` (`bench/udp-echo.sh`, one server core, 64-byte
datagrams over veth, 5 rounds of 8 seconds, medians with the range in `summary.txt`, every
round in `raw.csv`). `before-*` is branch perf/udp-faster (168d0f7); `after-*` is this
branch. Both were run in the same session, one after the other.

Echoes per second (median):

| | 16 tasks | 1 task |
|---|---:|---:|
| giong epoll, before | 331,931 | 358,820 |
| giong epoll, after | **394,104** | **368,732** |
| giong io_uring, before | 362,904 | 355,954 |
| giong io_uring, after | 355,204 | 347,126 |
| Tokio | 295,484 | 300,844 |
| kernel `recvmmsg`/`sendmmsg` × 32 | 421,493 | 431,274 |

Server CPU per echo (its process plus softirq on its core), µs:

| | 16 tasks | 1 task |
|---|---:|---:|
| giong epoll, before | 4.21 | 3.81 |
| giong epoll, after | **3.26** | **3.68** |
| giong io_uring, after | 3.72 | 3.92 |
| Tokio | 4.78 | 4.65 |
| kernel × 32 | 3.14 | 3.08 |

This VM varies by about 15% between runs (see the ranges); the CPU per echo moves less
than the rate.

## What was left, and what changed

`perf` on the server before the change: 94% of its CPU in the kernel, and of that two
things the kernel-only server does not pay.

1. **One `sendto(2)` per datagram.** The server made 1.0 system call per echo (Tokio 2.0).
   Datagrams sent while other tasks of the worker are runnable now wait for the end of
   that turn of the event loop and go to the kernel in one `sendmmsg(2)` per socket
   (`src/net/sendq.rs`); each sender still gets its own `sendto` result, and a task that is
   the only one with work sends at once. With 16 tasks: 0.06 system calls per echo
   (`epoll_pwait2`, `recvmmsg`, `sendmmsg` once per turn), 17 times fewer. On its own
   this was worth little (326,000 to 337,000 in a 6-second run): the system calls were
   not where the time went.
2. **Wake-ups of the epoll wait queue.** A descriptor in an epoll set has an entry on the
   socket's wait queue, so every datagram that arrives and every datagram sent and freed
   (`sock_wfree`, through `sock_def_write_space`) takes the queue's lock and calls epoll's
   callback: 7.1% of the server's CPU in `sock_wfree` against 1.5% for the kernel-only
   server, plus 2.4% more in `sock_def_readable`. A datagram socket that has received 64
   datagrams since it was added now leaves the set at its next event, and comes back
   (`EPOLL_CTL_ADD`, which reports readiness already there) only when a task has to wait
   for it. A socket that stays busy pays for no wake-ups at all; one that goes quiet pays
   at most two `epoll_ctl` per 64 datagrams. Streams never leave the set. This took the
   16-task run from 337,000 to about 395,000.

What is left against the kernel-only server is giong's own code (about 6% of the CPU:
the tasks, the waker queue, copying each datagram into the task's buffer) and the epoll
callback on the first datagram after a quiet spell. The rest of the time is the kernel's
UDP and veth path, which every server on kernel sockets pays: going past the ceiling
needs AF_XDP, which is planned after 1.0.

io_uring is unchanged within noise: its sends were already cheap, and its multishot
receive keeps a poll entry on the socket's wait queue, which it pays the same way. The
same "leave while busy" step for io_uring (cancel the multishot receive while datagrams
keep coming, read them directly) is the next thing to try there.
