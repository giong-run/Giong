# Core primitives: giong against the other Rust runtimes

`bench/core.sh 5` on the 4 vCPU cloud VM (Intel Xeon @ 2.10 GHz, Linux 6.18, Rust 1.97),
every run pinned to cores 1-2. Median wall-clock ns per operation over 5 rounds; lower is
better. `raw.csv` in each directory has every run (wall and CPU time), `summary.txt` the
medians with min and max.

- `io_uring/`: giong with this change and every other runtime.
- `io_uring-before/`: giong on the same tree without it (main + #142, #134, #133).
- `epoll/`, `epoll-before/`: giong with `GIONG_DRIVER=epoll`, with and without it.

| workload | giong before | giong | giong epoll before | giong epoll | tokio-ct | tokio-local | tokio-mt | monoio | glommio | compio | smol |
|---|---|---|---|---|---|---|---|---|---|---|---|
| spawn | 184 | **91.5** | 165 | **93.3** | 226 | 174 | 526 | 59.1 | 91.5 | 89.9 | 272 |
| yield | 24.3 | **23.7** | 28.4 | **28.7** | 62.3 | 73.5 | 70.5 | 49.5 | 45.0 | 30.5 | 138 |
| pingpong | 226 | **224** | 531 | **222** | 481 | 528 | 10,945 | 204 | 175 | 655 | 1,946 |
| mpsc | 40.3 | **30.9** | 41.0 | **24.3** | 61.8 | 57.9 | 141 | 13.8 | 23.4 | 118 | 109 |
| bounded | 48.9 | **48.0** | 47.0 | **44.9** | 94.8 | 95.0 | 216 | 25.1 | 45.1 | 104 | 116 |
| oneshot | 350 | **293** | 637 | **276** | 527 | 533 | 18,203 | 218 | 208 | 415 | 1,793 |
| chain | 270 | **220** | 286 | **224** | 334 | 289 | 734 | 91.8 | 343 | 585 | 484 |
| timers | 748 | **688** | 850 | **736** | 935 | 886 | 967 | 1,497 | 1,328 | 951 | 1,231 |
| timeout | 28.9 | **35.6** | 28.4 | **30.5** | 49.0 | 45.5 | 45.6 | 56.0 | 53.3 | 79.8 | 30.8 |
| xthread | 194 | **64.0** | 216 | **68.9** | 167 | 155 | 208 | 185 | 170 | 223 | 211 |
| tcp-rr | 4,925 | **4,652** | 5,741 | **5,304** | 5,231 | 5,280 | 31,219 | 5,181 | 13,517 | 5,257 | 8,851 |
| connect | 15,245 | **17,211** | 17,485 | **17,254** | 18,741 | 16,888 | 38,918 | 16,864 | 24,281 | 15,309 | 19,275 |
| lookup | 181 | **171** | 174 | **186** | 29,126 | 29,515 | 28,564 |  |  |  | 28,643 |

What the workloads do (bench/core/src/main.rs): `spawn` spawns and joins tiny tasks in
batches of 1,000; `yield` is 64 tasks yielding (one poll); `pingpong` two tasks bounce a
value over two unbounded channels; `mpsc` one producer task streams into an unbounded
channel; `bounded` four producers into a channel of 128; `oneshot` a oneshot answered by a
new task; `chain` 1,000 tasks pass a token along oneshots; `timers` 100,000 tasks each sleep
once (1 to 16 ms); `timeout` a deadline on a future that is ready at once; `xthread` a
plain thread feeds a task over the runtime's cross-thread channel; `tcp-rr` one byte back
and forth over a loopback TCP connection with both ends on one thread; `connect` connect,
accept and close; `lookup` resolves `localhost` (Tokio and smol call `getaddrinfo` on a
blocking thread every time; giong reads /etc/hosts once and caches).

Each runtime uses its own scheduler and timers, and its own channels where it ships them:
Tokio's `tokio::sync`, monoio's `local-sync` (by monoio's authors), glommio's local
channels; compio and smol use `async-channel` and `futures-channel` oneshots.

Reading it:

- giong is ahead of Tokio in every workload, on both drivers, except `tcp-rr` on epoll,
  where both make the same system calls (`recvfrom`, `sendto`, `epoll_wait`) and are level
  within noise (5,304 against 5,231; ranges overlap).
- monoio and glommio stay ahead on `spawn`, `mpsc`, `bounded`, `oneshot` and `chain`:
  their tasks and channels only work within one thread, with no atomic operations at all.
  giong's channels and oneshots also carry values between threads and workers (see
  `xthread`, where giong is 2.4 to 3.5 times faster than every other runtime).
- `connect` is limited by TIME_WAIT sockets piling up over a run and varies with the order
  of runs (min to max spans up to 50%). Interleaved runs of the two giong builds and Tokio
  right after each other: giong 13,600 to 14,600 ns, giong before 13,500 to 16,500, Tokio
  `LocalSet` 16,200 to 18,700.
- `timeout` measures only arming and dropping a timer (the future is ready first); the two
  giong builds run the same code there, and the difference is noise.
