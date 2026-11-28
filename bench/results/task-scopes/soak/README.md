# Soak with deadline scopes (PR #546, src at 553ddd4)

`GIONG_SOAK_SECS=6900 cargo test --release --test soak -- --ignored --nocapture`, once per
driver (`GIONG_DRIVER=epoll` for the second), in a 4-core cloud container, kernel 6.18.44.
`tests/soak.rs` at this commit adds two deadline scenarios to the usual churn: round trips
inside a 10 s scope deadline (one-off reads, linked on io_uring), and a slow reply (20 ms)
whose read a 2 ms scope deadline cuts, then read whole afterwards and compared.

| driver | connections | reads cut by a deadline, late reply intact | not checked (stall) | round trips under a deadline | RSS at 60 s / end | fds at end | unexpected |
|---|---|---|---|---|---|---|---|
| io_uring | 135,848,003 | 13,582,459 | 2,709 | 13,587,472 | 15,428 / 16,464 KiB | 23 | 0 |
| epoll | 135,885,231 | 13,587,530 | 1,024 | 13,591,610 | 7,828 / 8,116 KiB | 25 | 0 |

"Not checked": the worker stalled for about as long as the server takes to reply (longest
stalls 311 ms on io_uring, 209 ms on epoll, the machine's own: a plain thread stalled 308 ms
and 107 ms), so the read may have reached the kernel only after the reply and taken it, as a
dropped one-off read does on io_uring (see `giong::task`). An earlier epoll run was clean for
2,710 s when the container restarted; the log here is the full rerun.
